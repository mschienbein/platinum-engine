//! Layer III front half of `MP3DecoderWrapper_SpiritDSP::DecodeFrame` (0x13d768): side-info parse, scale
//! factors, Huffman decode + requantization.  Line references `Lnnn` are lines of
//! `ghidra/out1/0013d768.c`.  Everything is integer arithmetic with Apple's exact wrap/shift semantics.

use super::bitbuf::BitBuf;
use super::tables::{
    A_MPEG1BITRATE as MPEG1_BITRATE, A_MPEG2BITRATE as MPEG2_BITRATE, G_AMPEG1SCFLEN as MPEG1_SCF_LEN,
    G_APREAMPTABLE as PREAMP, G_ASCFPARTITIONS as SCF_PARTITIONS, G_RLE, G_ANSAMPLERATERECIPROCAL as SAMPLE_RATE_RECIP,
    GETSCALEFACTORSIZEMPEG2_AMODS as MPEG2_MODS, GETSCALEFACTORSIZEMPEG2_INV_MUL as MPEG2_INV_MUL,
    GETSCALEFACTORSIZEMPEG2_INV_SCL as MPEG2_INV_SCL, MP3D_COUNT1_TABLE_A as COUNT1_A, MP3D_G_ARFRACPOWEROFTWO as FRAC_POW2,
    MP3D_G_ARPOWER43LOW as POW43_LOW, MP3D_G_ARPOWER43SHORT_NEW as POW43_SHORT, MP3D_G_HUFF,
};

/// `_mp3d_g_huff` as signed 16-bit entries (negative = leaf).
static HUFF: [i16; MP3D_G_HUFF.len()] = {
    let mut o = [0i16; MP3D_G_HUFF.len()];
    let mut i = 0;
    while i < o.len() {
        o[i] = MP3D_G_HUFF[i] as i16;
        i += 1;
    }
    o
};
use super::granule::*;

/// Status codes of the front half's error paths (Apple's `uVar25` at `LAB_0013f9a8`).
pub const ST_OK: u32 = 0;
/// "not enough reservoir data" -> DecodeFrame returns 0x41010002.
pub const ST_CONCEAL: u32 = 1;
/// needs more input -> 0x81010003.
pub const ST_NEED_INPUT: u32 = 2;
/// fatal / reset -> 0x81010002 + Reset.
pub const ST_RESET: u32 = 4;

/// Front half state: `S+0..0xd8` meta, BitBuf (+ reservoir), work buffer.
pub struct Front {
    pub meta: Meta,
    pub bb: BitBuf,
    pub work: Work,
}

impl Default for Front {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn sx8(v: u8) -> i32 {
    v as i8 as i32
}

impl Front {
    /// State after `ACMP3Decoder::Initialize` / `MP3DecoderWrapper_SpiritDSP::Reset` (0x13d700).
    pub fn new() -> Self {
        let mut f = Front { meta: Meta::default(), bb: BitBuf::new(), work: Work::new() };
        f.reset_state();
        f
    }

    /// Spirit-state part of `Reset`: everything from `S+4` is zeroed, then `*S = -1`, `S+0xd7 = 0xff`,
    /// bit buffer positions initialised.  The work buffer `W` is NOT touched (Apple never clears it).
    pub fn reset_state(&mut self) {
        self.meta = Meta::default();
        self.meta.set_u32(0, 0xffff_ffff);
        self.meta.b[META_SR_CACHE] = 0xff;
        self.bb = BitBuf::new();
    }

    /// Rebuild the sfb width tables for sample-rate index `sr_idx` (L636-L682).
    pub fn build_sfb_tables(&mut self, sr_idx: u32) {
        let rle = &G_RLE;
        let mut pi = rle[sr_idx as usize] as usize; // &_g_rle + rle[idx]*2 bytes  (u16 index)
        let mut mult: usize = 3;
        let mut p44: usize = 0xd6;
        let mut p45: usize;
        let mut second = false;
        loop {
            let mut acc: u32 = 0; // uVar78
            p45 = p44 - 1;
            self.meta.b[p44] = 0;
            let mut left: i32 = 0x120; // iVar24
            loop {
                let e = rle.get(pi).copied().unwrap_or(0) as u32;
                let v = e & 0xff;
                let mut cnt = ((e >> 8) & 0xff) + 1;
                loop {
                    let prod = acc.wrapping_mul(v);
                    cnt = cnt.wrapping_sub(1);
                    acc = v;
                    if cnt != 0 {
                        acc = prod.wrapping_add(0x80) >> 8;
                    }
                    let start = p45.wrapping_sub(mult - 1);
                    if start > 0xd6 {
                        break;
                    }
                    for k in 0..mult {
                        self.meta.b[start + k] = acc as u8;
                    }
                    left = left.wrapping_sub((acc.wrapping_mul(mult as u32)) as i32);
                    p45 = start.wrapping_sub(1);
                    if p45 > 0xd6 {
                        break;
                    }
                    if cnt <= 1 {
                        break;
                    }
                }
                pi += 1;
                if left == 0 || pi >= rle.len() || p45 > 0xd6 {
                    break;
                }
            }
            mult = 1;
            let was = second;
            second = true;
            p44 = p45;
            if was || p44 > 0xd6 {
                break;
            }
        }
        let n: usize = if sr_idx > 5 { 8 } else { 6 };
        self.meta.b.copy_within(0x98..0x98 + n, 0x70);
        let mut dst = 0x70 + n;
        let mut src = 0xb8usize;
        loop {
            let b = self.meta.b[src];
            self.meta.b[dst] = b;
            dst += 1;
            src += 1;
            if b == 0 {
                break;
            }
        }
        self.meta.b[META_SR_CACHE] = sr_idx as u8;
    }

    /// Frame start for Layer III (L599-L1127 region): sfb tables, side info for all granules/channels,
    /// reservoir/main-data pointer setup.  Returns Err(status) for the `LAB_0013f9a8` error exits.
    pub fn side_info(&mut self, hdr: u32) -> Result<(), u32> {
        let sr_idx = ((hdr >> 19 & 1) + (hdr >> 20 & 1)) * 3 + (hdr >> 10 & 3);
        if sr_idx != (self.meta.b[META_SR_CACHE] as i8 as i32 as u32) {
            self.build_sfb_tables(sr_idx);
        }
        let stereo = (hdr & 0xc0) != 0xc0;
        let mut nrec: u32 = if stereo { 2 } else { 1 }; // uVar25
        let (main_data_begin, mut scfsi_src, geom): (u32, u32, u32);
        if (hdr >> 19 & 1) == 0 {
            // LSF
            let v = self.bb.get_bits(nrec | 8);
            scfsi_src = 0;
            main_data_begin = v >> nrec;
            geom = 0x962;
        } else {
            nrec *= 2;
            main_data_begin = self.bb.get_bits(9);
            scfsi_src = self.bb.get_bits(nrec + 7);
            geom = 0x483;
        }
        let n_long_mixed = ((geom >> 4) & 0xf) as u8;
        let mut roff = META_REC;
        let mut count = nrec;
        loop {
            let mut tmp = scfsi_src << 4; // uVar60
            if (hdr & 0xc0) != 0xc0 {
                tmp = scfsi_src;
            }
            let p23 = self.bb.get_bits(0xc);
            self.meta.set_u16(roff + rec::PART2_3, p23 as u16);
            let bv = self.bb.get_bits(9);
            if 0x120 < bv as i32 {
                return Err(ST_RESET);
            }
            self.meta.set_u16(roff + rec::BIG_VALUES, bv as u16);
            let gg = self.bb.get_bits(8);
            self.meta.b[roff + rec::GLOBAL_GAIN] = gg as u8;
            let scf = self.bb.get_bits(geom >> 8);
            self.meta.set_u16(roff + rec::SCF_COMPRESS, scf as u16);
            self.meta.b[roff + rec::SFB_OFF] = (0x98usize.wrapping_sub(roff)) as u8;
            self.meta.b[roff + rec::N_LONG] = 0x16;
            self.meta.b[roff + rec::N_SHORT] = 0;
            let ws = self.bb.get_bits(1);
            let sel: u32; // uVar72
            if ws == 0 {
                self.meta.b[roff + rec::BLOCK_TYPE] = 0;
                self.meta.b[roff + rec::MIXED] = 0;
                sel = self.bb.get_bits(0xf);
                self.meta.b[roff + 0xf] = self.bb.get_bits(4) as u8;
                self.meta.b[roff + 0x10] = self.bb.get_bits(3) as u8;
                self.meta.b[roff + 0x11] = 0x3f;
            } else {
                let bt = self.bb.get_bits(3); // block_type<<1 | mixed
                self.meta.b[roff + rec::MIXED] = (bt & 1) as u8;
                self.meta.b[roff + rec::BLOCK_TYPE] = ((bt as i32) >> 1) as u8;
                self.meta.b[roff + 0xf] = 0x07;
                self.meta.b[roff + 0x10] = 0x3f;
                if bt < 2 {
                    return Err(ST_RESET);
                }
                if ((bt as i32) >> 1) == 2 {
                    let (cnt_short, tab_off, n_long);
                    if (bt & 1) == 0 {
                        cnt_short = 0x27u8;
                        self.meta.b[roff + 0xf] = 8;
                        tab_off = 0xafusize;
                        n_long = 0u8;
                    } else {
                        cnt_short = 0x1e;
                        tab_off = 0x70;
                        n_long = n_long_mixed;
                    }
                    tmp &= 0xf0f;
                    self.meta.b[roff + rec::SFB_OFF] = tab_off.wrapping_sub(roff) as u8;
                    self.meta.b[roff + rec::N_LONG] = n_long;
                    self.meta.b[roff + rec::N_SHORT] = cnt_short;
                }
                sel = self.bb.get_bits(10) << 5;
                let sbg = self.bb.get_bits(9);
                self.meta.set_u16(roff + rec::SUBBLOCK_GAIN, sbg as u16);
            }
            self.meta.b[roff + rec::TABLE_SELECT] = (sel >> 10) as u8;
            self.meta.b[roff + rec::TABLE_SELECT + 1] = ((sel >> 5) & 0x1f) as u8;
            self.meta.b[roff + rec::TABLE_SELECT + 2] = (sel & 0x1f) as u8;
            let pre: u8 = if self.meta.u16(roff + rec::SCF_COMPRESS) < 500 { 0 } else { 4 };
            let low = self.bb.get_bits(geom & 3) as u8;
            self.meta.b[roff + rec::FLAGS] = pre.wrapping_add(low);
            self.meta.b[roff + rec::SCFSI] = ((tmp >> 12) & 0xf) as u8;
            scfsi_src = tmp << 4;
            roff += REC_LEN;
            count -= 1;
            if count == 0 {
                break;
            }
        }
        // main_data_begin is never negative (u32 from <= 9 bits)
        let framesize_f6c1 = self.bb.f6c1;
        if main_data_begin == 0 {
            let sz = frame_size_bytes(hdr, framesize_f6c1);
            self.bb.end = self.bb.end.wrapping_add(sz);
        } else {
            let sidebytes: u32 = {
                let base: u32 = if (hdr & 0x80000) != 0 { 0x1e } else { 0xf };
                (base >> (if (!hdr & 0xc0) == 0 { 1 } else { 0 })) + 6
            };
            let crc: u32 = (!(hdr >> 15)) & 2;
            let fill = self.bb.fill;
            let end = self.bb.end;
            let hb = sidebytes.wrapping_add(crc); // iVar23
            let mut pos = end.wrapping_add(hb); // uVar25
            if (fill.wrapping_sub(pos) as i32) < 0 {
                return Err(ST_CONCEAL);
            }
            let mut end_keep = end; // uVar29
            if fill != pos {
                let cnt = (fill - pos) as usize;
                let src = pos as usize;
                let dst = end as usize;
                for k in 0..cnt {
                    let v = self.bb.buf.get(src + k).copied().unwrap_or(0);
                    if dst + k < self.bb.buf.len() {
                        self.bb.buf[dst + k] = v;
                    }
                }
                pos = fill;
                end_keep = end;
            }
            self.bb.fill = pos.wrapping_sub(hb);
            let sz = frame_size_bytes(hdr, framesize_f6c1);
            self.bb.end = end_keep.wrapping_sub(hb).wrapping_add(sz);
            if (self.bb.reserve as i32) < (main_data_begin as i32) {
                let mut p = end.wrapping_sub(self.bb.reserve);
                p &= !((p as i32 >> 31) as u32);
                self.bb.load_at_byte(p);
                return Err(ST_CONCEAL);
            }
            let p = end.wrapping_sub(main_data_begin);
            self.bb.load_at_byte(p);
        }
        Ok(())
    }

    /// Decode one channel of one granule (`LAB_0013e82c`..`LAB_0013f1e0`).  `rec_off` = record offset in `meta`,
    /// `ch` = channel index.  Reads scale factors, Huffman data, count1; writes the requantized spectrum into the
    /// work buffer and `meta.count(ch)`; repositions the bit buffer at the end of the granule/channel data.
    pub fn channel(&mut self, ring: &[u8], hdr: u32, roff: usize, ch: usize) -> Result<(), u32> {
        let n_long = self.meta.b[roff + rec::N_LONG] as usize; // bVar19
        let n_short = self.meta.b[roff + rec::N_SHORT] as usize; // bVar10
        let p23 = self.meta.u16(roff + rec::PART2_3) as i32;
        let r = self.bb.ensure_bits(ring, p23);
        if r != 0 {
            return Err(r);
        }
        let spec0 = SPEC_BASE + ch * CH_STRIDE; // local_12e0 (int index in work)
        let gain0 = (spec0 + 0x240) - 1 - (n_long + n_short); // puVar34 (int index in work)
        let mut scfsi = self.meta.b[roff + rec::SCFSI] as u32; // uVar78
        let mut slen = [0xaau8; 4]; // local_a8
        let psel: usize; // partition table base inside SCF_PARTITIONS
        let mut part_off: usize;
        if self.meta.b[roff + rec::N_SHORT] == 0 {
            psel = 0;
        } else if self.meta.b[roff + rec::N_LONG] != 0 {
            psel = 0x38;
        } else {
            psel = 0x1c;
        }
        part_off = psel;
        let sfc = self.meta.u16(roff + rec::SCF_COMPRESS) as u32;
        if (hdr >> 19 & 1) == 0 {
            // LSF
            let is_r = (if ch != 0 { 1u32 } else { 0 }) & ((hdr & 0x10) >> 4); // uVar26
            let mut row: u32 = if is_r == 0 { 0 } else { 12 }; // uVar78
            let mut val = (sfc >> is_r) as i32; // uVar26
            loop {
                let mut prod: i32 = 1;
                let mut cur = val as u32; // uVar60
                for k in (0..4usize).rev() {
                    let m = MPEG2_MODS[k + row as usize] as usize;
                    let q = cur.wrapping_mul(MPEG2_INV_MUL[m] as u32) >> (MPEG2_INV_SCL[m] as u32 & 0x1f);
                    prod = prod.wrapping_mul(m as i32);
                    slen[k] = (cur as u8).wrapping_sub((q as u8).wrapping_mul(m as u8));
                    cur = q;
                }
                row += 4;
                val = val.wrapping_sub(prod);
                if val < 0 {
                    break;
                }
            }
            part_off += row as usize;
            scfsi = 0xffff_fff0;
        } else {
            let t = MPEG1_SCF_LEN[sfc as usize & 0xf];
            slen[0] = t >> 2;
            slen[1] = slen[0];
            slen[2] = t & 3;
            slen[3] = slen[2];
        }
        // Scale-factor reads
        let mut gi = gain0; // puVar56 index
        let mut sv = SCF_BASE + ch * SCF_STRIDE; // pcVar46 (byte offset in work)
        for p in 0..4usize {
            let cnt = SCF_PARTITIONS.get(part_off + p).copied().unwrap_or(0) as u32;
            let sl = slen[p] as u32;
            if cnt != 0 {
                if (scfsi >> 3 & 1) == 0 {
                    if sl == 0 {
                        for _ in 0..cnt {
                            self.work.w[gi] = 0;
                            self.work.set_byte(sv, 0);
                            gi += 1;
                            sv += 1;
                        }
                    } else {
                        let mut illegal: u32 = 0xffff_ffff;
                        if scfsi > 0x7fff_ffff {
                            illegal = !(0xffff_ffffu32.wrapping_shl(sl));
                        }
                        for _ in 0..cnt {
                            let v = self.bb.get_bits(sl);
                            let mut b = v;
                            if v == illegal {
                                b = 0xffff_ffff;
                            }
                            self.work.set_byte(sv, b as u8);
                            self.work.w[gi] = v as i32;
                            gi += 1;
                            sv += 1;
                        }
                    }
                } else {
                    for _ in 0..cnt {
                        self.work.w[gi] = sx8(self.work.byte(sv));
                        gi += 1;
                        sv += 1;
                    }
                }
            }
            scfsi <<= 1;
        }
        self.work.w[gi] = 0;
        self.work.w[gi + 1] = 0;
        self.work.w[gi + 2] = 0;
        // preflag / subblock gain / global gain
        let flags = self.meta.b[roff + rec::FLAGS]; // bVar19
        let scale_sh: u32 = if (flags & 2) != 0 { 2 } else { 1 }; // uVar78
        if n_short == 0 {
            if (flags >> 2 & 1) != 0 {
                for k in 0..10usize {
                    let i = gain0 + 0xb + k;
                    self.work.w[i] = self.work.w[i].wrapping_add(PREAMP[k] as i32);
                }
            }
        } else {
            let sbg = self.meta.u16(roff + rec::SUBBLOCK_GAIN) as u32; // uVar11
            if sbg != 0 {
                let sh = scale_sh ^ 3;
                let a = ((sbg >> 6) & 7) << sh;
                let b = ((sbg >> 3) & 7) << sh;
                let c = (sbg & 7) << sh;
                let mut i = gain0 + n_long;
                let mut cnt = n_short as u32;
                loop {
                    self.work.w[i] = self.work.w[i].wrapping_add(a as i32);
                    self.work.w[i + 1] = self.work.w[i + 1].wrapping_add(b as i32);
                    self.work.w[i + 2] = self.work.w[i + 2].wrapping_add(c as i32);
                    i += 3;
                    cnt = cnt.wrapping_sub(3);
                    if cnt == 0 {
                        break;
                    }
                }
            }
        }
        let base: i32 = if (hdr & 0xe0) != 0x60 { -0x6e } else { -0x70 };
        let gg = self.meta.b[roff + rec::GLOBAL_GAIN] as i32;
        for k in 0..(n_long + n_short) {
            let i = gain0 + k;
            let v = (self.work.w[i] as u32).wrapping_shl(scale_sh) as i32;
            self.work.w[i] = (base + gg).wrapping_sub(v);
        }
        self.huffman(hdr, roff, ch, gain0, flags)?;
        Ok(())
    }

    /// Huffman big_values + count1 + tail fill for one channel (L1010-L1405).
    fn huffman(&mut self, _hdr: u32, roff: usize, ch: usize, gain0: usize, flags: u8) -> Result<(), u32> {
        let spec0 = SPEC_BASE + ch * CH_STRIDE;
        let tab = self.meta.table_off(roff); // local_1308 (byte offset in meta)
        let mut wi = tab; // width pointer (index into meta.b)
        let mut gi = gain0; // puVar34
        let big_values = self.meta.u16(roff + rec::BIG_VALUES) as u32;
        let bb = &self.bb;
        let mut cache = bb.cache;
        let mut bpos = bb.bitpos;
        let mut ptr = bb.ptr;
        let mut off: usize = 0; // uVar72 (byte offset into spectrum)
        let mut g_last: u32; // local_1324
        let mut rem: i32; // uVar26
        macro_rules! refill {
            () => {
                if bpos >= 0 {
                    cache |= bb.rd16(ptr).wrapping_shl(bpos as u32);
                    ptr += 2;
                    bpos -= 16;
                }
            };
        }
        macro_rules! add {
            ($n:expr) => {
                let n: u32 = $n;
                cache = cache.wrapping_shl(n);
                bpos = bpos.wrapping_add(n as i32);
                refill!();
            };
        }
        let spec = &mut self.work.w;
        if big_values == 0 {
            g_last = 0;
            off = 0;
            rem = 1;
        } else {
            let mut regcnt: i32 = 0; // uVar29
            let mut region: usize = 0; // uVar54
            let mut linbits: u32 = 0; // uVar60
            let mut tree: Option<usize> = None; // puVar48 (index into HUFF)
            let mut left: i32 = big_values as i32; // uVar53
            loop {
                let wd: i32;
                let n_this: i32;
                let zero_region: bool;
                if regcnt < 1 {
                    regcnt = self.meta.b[roff + rec::REGION + region] as i32;
                    let sel = self.meta.b[roff + rec::TABLE_SELECT + region] as usize;
                    let e = HUFF[sel] as u16 as u32;
                    linbits = e & 0xf;
                    region += 1;
                    wd = self.meta.b[wi] as i32;
                    rem = left.wrapping_sub(wd);
                    g_last = spec[gi] as u32;
                    if e < 0x10 {
                        zero_region = true;
                        n_this = 0;
                    } else {
                        tree = Some(((e >> 4) & 0xfff) as usize);
                        zero_region = false;
                        n_this = if wd <= left { wd } else { left };
                    }
                } else {
                    regcnt -= 1;
                    wd = self.meta.b[wi] as i32;
                    rem = left.wrapping_sub(wd);
                    n_this = if wd <= left { wd } else { left };
                    g_last = spec[gi] as u32;
                    zero_region = tree.is_none();
                }
                if zero_region {
                    let x = (rem & (rem >> 31)).wrapping_add(wd) as u32;
                    let bytes = (x.wrapping_sub(1) as u64) * 8 + 8;
                    let nb = if x == 0 { 0 } else { bytes as usize };
                    let st = spec0 + off / 4;
                    let en = (st + nb / 4).min(spec.len());
                    for i in st..en {
                        spec[i] = 0;
                    }
                    off += nb;
                    tree = None;
                } else {
                    let tr = tree.unwrap_or(0);
                    let gl = g_last as i32;
                    let sh14: u32 = 0x15u32.wrapping_sub((gl >> 2) as u32);
                    let sh15: u32 = 0x19u32.wrapping_sub((gl >> 2) as u32);
                    let frac = g_last & 3;
                    let p43 = (frac << 4) as usize;
                    let fp = FRAC_POW2[frac as usize] as u32 as i32 as i64;
                    let mut pairs = n_this;
                    loop {
                        // tree walk
                        let mut ent: u32 = HUFF.get(tr + (cache >> 27) as usize).copied().unwrap_or(0) as i32 as u32;
                        if (ent as i32) >= 0 {
                            let mut sh: u32 = 5;
                            loop {
                                add!(sh);
                                sh = ent & 7;
                                let ix = (cache.wrapping_shr(0u32.wrapping_sub(sh) & 0x1f)).wrapping_add(ent >> 3) as usize;
                                ent = HUFF.get(tr + ix).copied().unwrap_or(0) as i32 as u32;
                                if (ent as i32) < 0 {
                                    break;
                                }
                            }
                        }
                        add!(ent & 7);
                        // x value
                        let mut vals = [0u32; 2];
                        for (k, val) in vals.iter_mut().enumerate() {
                            let q = if k == 0 { (ent >> 3) & 0xf } else { (ent >> 7) & 0xf };
                            if q == 0 {
                                *val = 0;
                                continue;
                            }
                            let mag: u32;
                            if q == 0xf && linbits != 0 {
                                let lb = cache.wrapping_shr(0x20u32.wrapping_sub(linbits) & 0x1f);
                                add!(linbits);
                                if lb < 0x74 {
                                    mag = if sh14 < 0x20 {
                                        let p = POW43_SHORT[lb as usize] as i32 as i64 * fp;
                                        ((p as u64 >> 15) as u32) >> (sh14 & 0x1f)
                                    } else {
                                        0
                                    };
                                } else {
                                    let adj: i32 = if 0x3f0 < lb { -8 } else { -4 };
                                    let u = if 0x3f0 < lb { lb.wrapping_add(0xf) } else { lb.wrapping_add(0xf).wrapping_mul(8) };
                                    let idx = ((u >> 6).wrapping_sub(0xf)) as usize;
                                    let f = 0x40i32.wrapping_sub((u & 0x3f) as i32);
                                    let a = POW43_SHORT[idx] as i32;
                                    let b = POW43_SHORT[idx + 1] as i32;
                                    let c = POW43_SHORT[idx + 2] as i32;
                                    let t1 = c.wrapping_add(a).wrapping_sub(b.wrapping_mul(2)).wrapping_mul(f) >> 6;
                                    let t2 = a.wrapping_sub(c).wrapping_add(t1).wrapping_mul(f) >> 7;
                                    let inner = b.wrapping_add(t2);
                                    let ash = (adj as u32).wrapping_add(sh14);
                                    mag = if ash < 0x20 {
                                        let p = inner as i64 * fp;
                                        ((p as u64 >> 15) as u32) >> (ash & 0x1f)
                                    } else {
                                        0
                                    };
                                }
                            } else {
                                mag = if sh15 < 0x20 { POW43_LOW[q as usize + p43] >> (sh15 & 0x1f) } else { 0 };
                            }
                            let s = if (cache as i32) >= 0 { mag } else { mag.wrapping_neg() };
                            cache = cache.wrapping_shl(1);
                            bpos = bpos.wrapping_add(1);
                            *val = s;
                        }
                        let i = spec0 + off / 4;
                        spec[i] = vals[0] as i32;
                        spec[i + 1] = vals[1] as i32;
                        off += 8;
                        pairs -= 1;
                        if pairs == 0 {
                            break;
                        }
                    }
                }
                wi += 1;
                gi += 1;
                left = rem;
                if rem <= 0 {
                    break;
                }
            }
            rem = 1i32.wrapping_sub(rem);
        }
        // count1 region
        let mut limit: i32 = (self.bb.seg_end as i32).wrapping_add((0i32.wrapping_sub(ptr as i32)).wrapping_mul(8)).wrapping_add(0x10); // iVar23
        let mut done_first_pair_only = false;
        if bpos < limit {
            let g1 = g_last;
            let sh = 0x19u32.wrapping_sub((g1 as i32 >> 2) as u32);
            let mut one: u32 = if sh < 0x20 { POW43_LOW[((g1 & 3) << 4 | 1) as usize] >> (sh & 0x1f) } else { 0 };
            loop {
                let i = spec0 + off / 4;
                let flags4: u32;
                if (flags & 1) == 0 {
                    if (cache as i32) < 0 {
                        flags4 = 0;
                        cache = cache.wrapping_shl(1);
                        bpos = bpos.wrapping_add(1);
                    } else {
                        let b = COUNT1_A[(cache >> 26) as usize] as u32;
                        flags4 = b;
                        let n = b >> 4;
                        cache = cache.wrapping_shl(n);
                        bpos = bpos.wrapping_add(n as i32);
                    }
                } else {
                    flags4 = !(cache >> 28);
                    cache = cache.wrapping_shl(4);
                    bpos = bpos.wrapping_add(4);
                }
                if bpos >= 0 {
                    cache |= bb.rd16(ptr).wrapping_shl(bpos as u32);
                    ptr += 2;
                    bpos -= 16;
                    limit = limit.wrapping_sub(0x10);
                }
                if limit <= bpos {
                    break;
                }
                rem = rem.wrapping_sub(1);
                if rem == 0 {
                    let w = self.meta.b[wi] as i32;
                    wi += 1;
                    rem = w;
                    if rem == 0 {
                        break;
                    }
                    let g = spec[gi];
                    gi += 1;
                    let sh = 0x19u32.wrapping_sub((g >> 2) as u32);
                    one = if sh < 0x20 { POW43_LOW[(((g as u32) & 3) << 4 | 1) as usize] >> (sh & 0x1f) } else { 0 };
                }
                let sgn = |c: u32, o: u32| -> u32 { if (c as i32) < 0 { o.wrapping_neg() } else { o } };
                let mut v = [0u32; 4];
                // value 0 (bit 3)
                let s0 = sgn(cache, one);
                if flags4 & 8 != 0 {
                    bpos = bpos.wrapping_add(1);
                    cache = cache.wrapping_shl(1);
                    v[0] = s0;
                }
                let s1 = sgn(cache, one);
                if flags4 & 4 != 0 {
                    cache = cache.wrapping_shl(1);
                    bpos = bpos.wrapping_add(1);
                    v[1] = s1;
                }
                spec[i] = v[0] as i32;
                spec[i + 1] = v[1] as i32;
                rem = rem.wrapping_sub(1);
                if rem == 0 {
                    let w = self.meta.b[wi] as i32;
                    wi += 1;
                    rem = w;
                    if rem == 0 {
                        done_first_pair_only = true;
                        break;
                    }
                    let g = spec[gi];
                    gi += 1;
                    let sh = 0x19u32.wrapping_sub((g >> 2) as u32);
                    one = if sh < 0x20 { POW43_LOW[(((g as u32) & 3) << 4 | 1) as usize] >> (sh & 0x1f) } else { 0 };
                }
                let s2 = sgn(cache, one);
                if flags4 & 2 != 0 {
                    bpos = bpos.wrapping_add(1);
                    cache = cache.wrapping_shl(1);
                    v[2] = s2;
                }
                let s3 = sgn(cache, one);
                if flags4 & 1 != 0 {
                    bpos = bpos.wrapping_add(1);
                    cache = cache.wrapping_shl(1);
                    v[3] = s3;
                }
                spec[i + 2] = v[2] as i32;
                spec[i + 3] = v[3] as i32;
                off += 0x10;
            }
        }
        if done_first_pair_only {
            off += 8;
        }
        // tail zero fill (L1386-L1395)
        if off < 0x900 {
            let upto = off.max(0x8f8) + 8; // bytes from off: (max(off,0x8f8) - off) + 8
            let n = upto - off;
            let st = spec0 + off / 4;
            let en = (st + n / 4).min(spec.len());
            for x in spec[st..en].iter_mut() {
                *x = 0;
            }
        }
        self.meta.set_u16(META_COUNT + 2 * ch, (off >> 2) as u16);
        let se = self.bb.seg_end;
        self.bb.load_at(se);
        Ok(())
    }
}


/// `_mp3d_FrameHeaderDataSizeInBytes(hdr, f6c1)` (0x2d6250).
pub fn frame_size_bytes(hdr: u32, f6c1: u32) -> u32 {
    let mut param2 = f6c1;
    if (hdr & 0xf000) != 0 {
        let layer = hdr >> 17 & 3;
        let mut l1: u64 = 0xc;
        if layer != 3 {
            l1 = 0x24;
        }
        let mut coef: u64 = 0x12;
        if layer != 1 || (hdr & 0x80000) != 0 {
            coef = l1;
        }
        let br = if (hdr & 0x80000) != 0 {
            MPEG1_BITRATE[((layer * 15).wrapping_sub(15) as usize) + (hdr >> 12 & 0xf) as usize]
        } else if (hdr & 0x60000) != 0x60000 {
            MPEG2_BITRATE[(hdr >> 12 & 0xf) as usize]
        } else {
            MPEG2_BITRATE[15 + (hdr >> 12 & 0xf) as usize]
        } as u64;
        let sh: u32 = (if (hdr & 0x80000) != 0 { 2 } else { 1 }) & (((hdr << 11) as i32) >> 31) as u32;
        let rcp = SAMPLE_RATE_RECIP[(hdr >> 10 & 3) as usize] as u64;
        let v = (((coef * br * rcp) >> 10) as u32) >> sh;
        param2 = v & 0xffff_fffc;
        if (hdr & 0x60000) != 0x60000 {
            param2 = v;
        }
    }
    let mut v = param2.wrapping_add(4);
    if (!hdr & 0x60000) != 0 {
        v = param2.wrapping_add(1);
    }
    if (hdr & 0x200) != 0 {
        param2 = v;
    }
    param2
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tables::{SFB_LONG_HALF_WIDTHS, SFB_SHORT_HALF_WIDTHS};

    /// `build_sfb_tables` reproduces the independently derived (Tables) band width tables for all 9 rate indices.
    #[test]
    fn sfb_tables_match_derived_widths() {
        for sr in 0..9u32 {
            let mut f = Front::new();
            f.build_sfb_tables(sr);
            let m = &f.meta.b;
            assert_eq!(&m[META_LONG_TAB..META_LONG_TAB + 22], &SFB_LONG_HALF_WIDTHS[sr as usize][..], "long sr {sr}");
            assert_eq!(m[0xae], 0);
            for i in 0..13 {
                for k in 0..3 {
                    assert_eq!(m[META_SHORT_TAB + 3 * i + k], SFB_SHORT_HALF_WIDTHS[sr as usize][i], "short sr {sr} sfb {i}");
                }
            }
            assert_eq!(m[0xd6], 0);
            let n = if sr > 5 { 8 } else { 6 };
            assert_eq!(&m[META_MIXED_TAB..META_MIXED_TAB + n], &SFB_LONG_HALF_WIDTHS[sr as usize][..n]);
            assert_eq!(m[META_SR_CACHE], sr as u8);
        }
    }

    /// Frame sizes of the common LAME layouts (hdr with padding 0): 128 kbps / 44.1 kHz = 417, 192 kbps / 48 kHz = 576.
    #[test]
    fn frame_sizes() {
        assert_eq!(frame_size_bytes(0xfffb_9000, 0), 417);
        assert_eq!(frame_size_bytes(0xfffb_9200, 0), 418);
        assert_eq!(frame_size_bytes(0xfffb_b400, 0), 576);
    }
}
