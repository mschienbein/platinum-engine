//! CONTRACT between the front half (`decoder.rs`/`front*.rs`, owner Mp3XFront) and the back half
//! (`back.rs`, owner Mp3XBack) of Apple's `MP3DecoderWrapper_SpiritDSP::DecodeFrame` (file 0x13d768,
//! decompile `ghidra/out1/0013d768.c`; all `Lnnn` below are lines of that .c file, `S` = the decoder
//! state object `*(this+0x2010)` of 0x1da8 bytes, `W` = the work buffer `*(this+0x2018)` of 0x12a0 bytes).
//!
//! Design rule: mirror Apple's memory layout so that every arithmetic index of the decompile can be
//! used verbatim.  Two flat arrays carry everything the two halves share:
//!
//! * [`Meta`] = bytes `S+0x00 .. S+0xd8` (counter, per-channel counts, 4 granule-info records,
//!   scale-factor-band width tables, sample-rate cache).  Record fields and the sfb tables are
//!   addressed by BYTE OFFSET relative to `S` (just like Apple's `lVar76 + rec[9]` pointer math).
//! * [`Work`] = the 0x12a0-byte work buffer, as 1192 `i32` (little endian), `piVar42` = `W`
//!   (8-aligned).  Spectrum channel `ch` lives at int index `SPEC_BASE + ch*CH_STRIDE`
//!   (`piVar4 = piVar42 + 0x12`, channel 1 = +0x240 ints).  The synthesis stage (Back) also uses `W`
//!   from int 0 as its scratch/PCM staging (`_mp3d_SubbandSynth(piVar42, ..)`).
//!
//! Everything else in `S` is private to one half:
//! * Front: bit buffer, reservoir buffer (S+0xd8..S+0x350), header word, resync counters.
//! * Back : IMDCT overlap `S+0x350 + ch*0x480` (288 words per channel, [`OVERLAP_WORDS`]) and the
//!   polyphase synthesis state at `S+8 -> ((S+0xc8c)&~63)`; both are zeroed by `Reset`.
//!   (Layer I/II scratch and the reservoir-buffer overrun alias this memory in Apple's layout; the front
//!   half models it through `BitBuf::tail`, which `decoder.rs` synchronizes with `BackEnd::overlap_mut`.)
//!
//! ## What the back end needs per granule (everything after `LAB_0013f1e8`, L1412..L1763)
//!
//! Per granule Front calls [`GranuleIn`]-based `BackEnd::granule` exactly where Apple falls through
//! to `LAB_0013f1e8` (all channels of the granule have been Huffman-decoded + requantized):
//!
//! ```text
//! pub struct BackEnd { overlap: [[u32;288];2] / synth state ... }          // back.rs (Mp3XBack)
//! impl BackEnd {
//!     pub fn new() -> Self;                 // == zeroed state (as Reset / Initialize)
//!     pub fn reset(&mut self);              // zero overlap + synth state
//!     /// L1412..L1760 + SubbandSynth + CopyPCMSamples{Stereo,Mono} (L1771..L1783) for one granule
//!     /// (MPEG-1: 2 per frame, MPEG-2/2.5: 1 per frame).  Returns samples PER CHANNEL written
//!     /// (576 for Layer III).  `pcm` receives interleaved int16 (L,R,L,R.. / mono).
//!     pub fn granule(&mut self, g: &mut GranuleIn<'_>, pcm: &mut [i16]) -> usize;
//!     /// Layer I/II (stretch): `_mp3d_SubbandSynth(W,S+8,nblocks,stereo)` + `CopyPCMSamples` only.
//!     pub fn synth_only(&mut self, work: &mut Work, nblocks: usize, stereo: bool, pcm: &mut [i16]) -> usize;
//! }
//! ```
//!
//! Front guarantees at the call (== Apple at `LAB_0013f1e8`):
//!
//! 1. `g.hdr` (S+0xe8): the 32-bit big-endian header of the CURRENT frame (sync in bits 31..21).
//!    Fields the back half reads (L1412.., L1560.., L1765..): `hdr&0xc0` (channel mode, `0xc0`=mono,
//!    `0x40`=joint), `hdr&0x30` (mode extension; bit4 = intensity, bit5 = MS), `hdr>>19&1` (MPEG-1),
//!    `hdr>>20&1`, `hdr>>10&3` sample-rate index (not directly; via `S+0xd7`).
//! 2. `g.meta`: [`Meta`], see accessor docs: counts, granule records of THIS granule at
//!    `g.rec0` (= S+0x18 + (gr*nch)*0x16, i.e. `lVar76`, record of channel 0; channel 1 at +0x16),
//!    sfb tables (`rec[9]` byte offset), `S+0xd7`.
//! 3. `g.work`: both channels' dequantized spectra, Apple's integer scale, in place:
//!    `W[SPEC_BASE + ch*CH_STRIDE + k]`, k in 0..576, **long-block order for long blocks, sfb-interleaved
//!    window order for short blocks (NOT yet reordered; reorder is Back's job, L1620..L1665)**;
//!    lines `>= count(ch)` are zero (tail zero fill, L1387..L1395), also lines in
//!    `[count(ch), 576)`.  Bytes `W+0x124c + ch*0x27 .. +0x27` hold the channel's scalefactors as left
//!    by the scale-factor decode (MPEG-1 scfsi source for the next granule; for intensity stereo the
//!    right channel's `is_pos` array is `W+0x1273` (`pbVar8`, L1425..L1560) which Back reads AND
//!    writes: these bytes persist into the next granule's scfsi copy exactly like Apple).
//! 4. Pre-existing Back state (overlap, synth) is whatever Back left after the previous granule.
//!
//! Back must leave in `meta`: `count(ch)`/`prev_count(ch)` updates (L1431..L1450 MS count max;
//! L1699..L1715 `S+0x14+2*ch`), exactly as Apple does; Front does not touch them afterwards except
//! in `Reset`/resync (zero fill, S+4..S+0x1da8).
//!
//! The granule counter `*S` (`meta.counter()`): Front owns it.  Back only needs the two completion
//! tests of L1756..L1763 which are handled by Front *around* `granule()` (Front sets the counter to
//! `0xffffffff` when the frame is done, before it calls `granule()` for the last granule; Back never
//! reads it).  This preserves Apple's order: stereo..IMDCT for all channels, THEN synthesis+PCM.

/// Size of the work buffer in bytes (`ACMP3Decoder::Initialize`: operator new[](0x12a0)).
pub const WORK_BYTES: usize = 0x12a0;
/// Work buffer as 32-bit words.
pub const WORK_INTS: usize = WORK_BYTES / 4;
/// Int index of channel 0 spectrum in the work buffer (`piVar4 = piVar42 + 0x12`).
pub const SPEC_BASE: usize = 0x12;
/// Distance between channel spectra in ints (0x240 = 576).
pub const CH_STRIDE: usize = 0x240;
/// Byte offset in the work buffer of channel 0's saved scalefactors; channel 1 at `+SCF_STRIDE`.
pub const SCF_BASE: usize = 0x124c;
/// Bytes per channel scalefactor save area (39 = 13 short sfb * 3 windows).
pub const SCF_STRIDE: usize = 0x27;
/// Byte offset of the right channel's saved scalefactors == the intensity-stereo `is_pos` array (`pbVar8`).
pub const ISPOS_BASE: usize = SCF_BASE + SCF_STRIDE; // 0x1273
/// Overlap words per channel (`puVar7 + ch*0x120`, `puVar7 = S+0x350`).
pub const OVERLAP_WORDS: usize = 0x120;

/// The 0x12a0-byte work buffer `W` (little-endian words).  Byte accessors address `W + off`.
#[derive(Clone)]
pub struct Work {
    pub w: [i32; WORK_INTS],
}

impl Default for Work {
    fn default() -> Self {
        Self::new()
    }
}

impl Work {
    /// Apple allocates it with `operator new[]` and never clears it (its content is whatever the
    /// heap held).  `Reset`/`Initialize` do not touch it.  We start zeroed; see decoder.rs notes.
    pub fn new() -> Self {
        Work { w: [0; WORK_INTS] }
    }
    /// Byte at `W + off`.
    #[inline]
    pub fn byte(&self, off: usize) -> u8 {
        (self.w[off >> 2] >> ((off & 3) * 8)) as u8
    }
    /// Store byte at `W + off`.
    #[inline]
    pub fn set_byte(&mut self, off: usize, v: u8) {
        let sh = (off & 3) * 8;
        let i = off >> 2;
        self.w[i] = (self.w[i] & !(0xff << sh)) | ((v as i32) << sh);
    }
    /// Spectrum slice of channel `ch` (576 ints, `piVar4 + ch*0x240`).
    #[inline]
    pub fn spec(&self, ch: usize) -> &[i32] {
        let b = SPEC_BASE + ch * CH_STRIDE;
        &self.w[b..b + CH_STRIDE]
    }
    #[inline]
    pub fn spec_mut(&mut self, ch: usize) -> &mut [i32] {
        let b = SPEC_BASE + ch * CH_STRIDE;
        &mut self.w[b..b + CH_STRIDE]
    }
}

/// Number of bytes mirrored from the start of `S`.
pub const META_LEN: usize = 0xd8;
/// `S+0x10` : u16 count of channel 0 (lines produced by Huffman+count1, in ints); channel 1 at +0x12.
pub const META_COUNT: usize = 0x10;
/// `S+0x14` : u16 running "previous count" per channel (max of count and previous), used for the
/// antialias / IMDCT subband limit; channel 1 at +0x16.
pub const META_PREV_COUNT: usize = 0x14;
/// `S+0x18` : first of 4 granule-info records (22 = 0x16 bytes each): index `gr*nch + ch`.
pub const META_REC: usize = 0x18;
/// Record size.
pub const REC_LEN: usize = 0x16;
/// `S+0x70` : sfb width table used by mixed blocks (6 or 8 long widths, then short widths from
/// short-sfb 3, zero-terminated).  Widths are in units of 2 lines (= Huffman pairs).
pub const META_MIXED_TAB: usize = 0x70;
/// `S+0x98` : long-block sfb width table (22 entries, `S+0xae` = 0 terminator).
pub const META_LONG_TAB: usize = 0x98;
/// `S+0xaf` : short-block sfb width table (13 sfb, each width repeated 3 times = 39 bytes, `S+0xd6` = 0).
pub const META_SHORT_TAB: usize = 0xaf;
/// `S+0xd7` : cached sample-rate/version index `((hdr>>19&1)+(hdr>>20&1))*3 + (hdr>>10&3)` (i8),
/// `0xff` after reset.  `== 2` (MPEG-2.5 8 kHz) makes the mixed-block long-subband count 4 instead of 2
/// in the antialias/IMDCT code (L1594: `*(char*)(S+0xd7) != 2` -> 2 else 4).
pub const META_SR_CACHE: usize = 0xd7;

/// Byte offsets inside a 0x16-byte granule-info record (`lVar76`/`puVar73`), with the decompile
/// provenance.  `hdr` fields in brackets are the bitstream order.
pub mod rec {
    /// u16 part2_3_length (12 bits) (L576 `GetBits(0xc)`).
    pub const PART2_3: usize = 0x00;
    /// u16 big_values (9 bits), `> 0x120` is an error (L578).
    pub const BIG_VALUES: usize = 0x02;
    /// u16 scalefac_compress (4 bits MPEG-1, 9 bits LSF).
    pub const SCF_COMPRESS: usize = 0x04;
    /// u8 global_gain.
    pub const GLOBAL_GAIN: usize = 0x06;
    /// u8 block_type (0 if no window switching, else 1,2,3). `==2` short, `==3` stop (L1738 `cVar37`).
    pub const BLOCK_TYPE: usize = 0x07;
    /// u8 mixed_block_flag (0/1).
    pub const MIXED: usize = 0x08;
    /// u8 offset of the sfb width table relative to the record start (byte-wrapping add:
    /// `table_off = rec_off + rec[9]`): long -> S+0x98, short -> S+0xaf, mixed -> S+0x70.
    pub const SFB_OFF: usize = 0x09;
    /// u8 number of long sfb entries: 22 long, 0 short, 6 (LSF) / 8 (MPEG-1) mixed.
    pub const N_LONG: usize = 0x0a;
    /// u8 number of short sfb entries (3 windows): 0 long, 39 short (0x27), 30 mixed (0x1e).
    pub const N_SHORT: usize = 0x0b;
    /// 3 x u8 table_select (rec[0xc..0xe]).
    pub const TABLE_SELECT: usize = 0x0c;
    /// 3 x u8 region sfb counts (region0_count, region1_count, region2 = 0x3f); `N+1` sfb per region.
    pub const REGION: usize = 0x0f;
    /// u16 subblock_gain, 3 x 3 bits packed (window switching only).
    pub const SUBBLOCK_GAIN: usize = 0x12;
    /// u8 flags: bit2 = preflag (MPEG-1 bit, LSF: scalefac_compress >= 500), bit1 = scalefac_scale,
    /// bit0 = count1table_select.
    pub const FLAGS: usize = 0x14;
    /// u8 scfsi nibble for this record (MPEG-1; includes Apple's quirk that granule-0 right channel
    /// receives the private bits).
    pub const SCFSI: usize = 0x15;
}

/// Little-endian accessors over the first 0xd8 bytes of `S`.
#[derive(Clone)]
pub struct Meta {
    pub b: [u8; META_LEN],
}

impl Default for Meta {
    fn default() -> Self {
        Meta { b: [0; META_LEN] }
    }
}

impl Meta {
    #[inline]
    pub fn u8(&self, off: usize) -> u8 {
        self.b[off]
    }
    #[inline]
    pub fn u16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.b[off], self.b[off + 1]])
    }
    #[inline]
    pub fn set_u16(&mut self, off: usize, v: u16) {
        self.b[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn u32(&self, off: usize) -> u32 {
        u32::from_le_bytes([self.b[off], self.b[off + 1], self.b[off + 2], self.b[off + 3]])
    }
    #[inline]
    pub fn set_u32(&mut self, off: usize, v: u32) {
        self.b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    /// `*S`: granule counter (Front private).
    #[inline]
    pub fn counter(&self) -> u32 {
        self.u32(0)
    }
    /// u16 at `S+0x10+2*ch`.
    #[inline]
    pub fn count(&self, ch: usize) -> u16 {
        self.u16(META_COUNT + 2 * ch)
    }
    #[inline]
    pub fn set_count(&mut self, ch: usize, v: u16) {
        self.set_u16(META_COUNT + 2 * ch, v)
    }
    /// u16 at `S+0x14+2*ch`.
    #[inline]
    pub fn prev_count(&self, ch: usize) -> u16 {
        self.u16(META_PREV_COUNT + 2 * ch)
    }
    #[inline]
    pub fn set_prev_count(&mut self, ch: usize, v: u16) {
        self.set_u16(META_PREV_COUNT + 2 * ch, v)
    }
    /// Offset of record number `idx` (`0..4`) inside `S`.
    #[inline]
    pub const fn rec_off(idx: usize) -> usize {
        META_REC + idx * REC_LEN
    }
    /// Offset of the sfb width table of the record at `rec_off` (byte wrapping add like Apple).
    #[inline]
    pub fn table_off(&self, rec_off: usize) -> usize {
        rec_off + self.b[rec_off + rec::SFB_OFF] as usize
    }
    /// Cached sample-rate index (`S+0xd7`), as i8.
    #[inline]
    pub fn sr_cache(&self) -> i8 {
        self.b[META_SR_CACHE] as i8
    }
}

/// Everything `BackEnd::granule` needs; see the module documentation.
pub struct GranuleIn<'a> {
    /// Header word of the current frame (S+0xe8).
    pub hdr: u32,
    /// `S+0 .. S+0xd8`.
    pub meta: &'a mut Meta,
    /// Offset in `meta` of the record of channel 0 of this granule (`lVar76 - S`).
    pub rec0: usize,
    /// Work buffer `W`.
    pub work: &'a mut Work,
}

impl GranuleIn<'_> {
    /// Number of channels (`hdr&0xc0 == 0xc0` is mono).
    #[inline]
    pub fn nch(&self) -> usize {
        if (self.hdr & 0xc0) == 0xc0 { 1 } else { 2 }
    }
}
