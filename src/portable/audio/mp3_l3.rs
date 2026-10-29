//! MPEG-1/2/2.5 Layer III frame decoder.
//!
//! Safe Rust, no platform math, no unsafe. All arithmetic is IEEE-754 binary64
//! `+ - * /` with a fixed evaluation order and no fused multiply-add, so the
//! output is bit-identical on every host. Hostile input never panics: all
//! counts are clamped to their structural limits and damaged granules decode
//! as silence.

use super::mp3_math::{tables, Tables, Tree, QUARTER_BIAS};

pub const GRANULE: usize = 576;
const MAX_MAIN_DATA: usize = 511;
const SQRT_HALF: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version { V1, V2, V25 }

/// A decoded 4-byte frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub raw: u32,
    pub version: Version,
    /// 1, 2 or 3.
    pub layer: u8,
    /// A 16-bit CRC follows the header.
    pub crc: bool,
    /// 0 means free format.
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub padding: bool,
    /// 0 stereo, 1 joint stereo, 2 dual channel, 3 mono.
    pub mode: u8,
    pub mode_ext: u8,
    pub emphasis: u8,
}

const BITRATES_V1_L1: [u16; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
const BITRATES_V1_L2: [u16; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
const BITRATES_V1_L3: [u16; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
const BITRATES_V2_L1: [u16; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
const BITRATES_V2_L23: [u16; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
const RATES: [[u32; 3]; 3] = [[44_100, 48_000, 32_000], [22_050, 24_000, 16_000], [11_025, 12_000, 8_000]];

impl Header {
    /// Validate and decode a header word. Reserved values are rejected;
    /// bitrate index 0 (free format) is accepted with `bitrate_kbps == 0`.
    pub fn parse(word: u32) -> Option<Header> {
        if word >> 21 != 0x7ff { return None; }
        let version = match (word >> 19) & 3 { 0 => Version::V25, 2 => Version::V2, 3 => Version::V1, _ => return None };
        let layer = match (word >> 17) & 3 { 1 => 3, 2 => 2, 3 => 1, _ => return None };
        let bitrate_index = ((word >> 12) & 15) as usize;
        let rate_index = ((word >> 10) & 3) as usize;
        if bitrate_index == 15 || rate_index == 3 { return None; }
        let table = match (version, layer) {
            (Version::V1, 1) => &BITRATES_V1_L1,
            (Version::V1, 2) => &BITRATES_V1_L2,
            (Version::V1, _) => &BITRATES_V1_L3,
            (_, 1) => &BITRATES_V2_L1,
            _ => &BITRATES_V2_L23,
        };
        let family = match version { Version::V1 => 0, Version::V2 => 1, Version::V25 => 2 };
        Some(Header {
            raw: word, version, layer, crc: (word >> 16) & 1 == 0,
            bitrate_kbps: u32::from(table[bitrate_index]), sample_rate: RATES[family][rate_index],
            padding: (word >> 9) & 1 == 1, mode: ((word >> 6) & 3) as u8, mode_ext: ((word >> 4) & 3) as u8,
            emphasis: (word & 3) as u8,
        })
    }
    pub fn channels(&self) -> usize { if self.mode == 3 { 1 } else { 2 } }
    /// PCM frames per channel in one MPEG frame.
    pub fn samples_per_frame(&self) -> usize {
        match (self.layer, self.version) { (1, _) => 384, (3, Version::V1) | (2, _) => 1152, _ => 576 }
    }
    /// Frame length in bytes including the header; 0 for free format.
    pub fn frame_bytes(&self) -> usize {
        if self.bitrate_kbps == 0 { return 0; }
        let pad = usize::from(self.padding);
        let br = self.bitrate_kbps as usize * 1000;
        let sr = self.sample_rate as usize;
        match (self.layer, self.version) {
            (1, _) => (12 * br / sr + pad) * 4,
            (3, Version::V1) | (2, _) => 144 * br / sr + pad,
            _ => 72 * br / sr + pad,
        }
    }
    pub fn side_info_bytes(&self) -> usize {
        match (self.version == Version::V1, self.channels()) { (true, 2) => 32, (true, _) => 17, (false, 2) => 17, (false, _) => 9 }
    }
    fn band_set(&self) -> usize {
        let family = match self.version { Version::V1 => 3, Version::V2 => 0, Version::V25 => 6 };
        let slot = match self.sample_rate { 44_100 | 22_050 | 11_025 => 0, 48_000 | 24_000 | 12_000 => 1, _ => 2 };
        family + slot
    }
}

// Scale-factor band edges. Index: [22.05, 24, 16, 44.1, 48, 32, 11.025, 12, 8] kHz.
pub(super) static SFB_LONG: [[u16; 23]; 9] = [
    [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
    [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 114, 136, 162, 194, 232, 278, 332, 394, 464, 540, 576],
    [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
    [0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 52, 62, 74, 90, 110, 134, 162, 196, 238, 288, 342, 418, 576],
    [0, 4, 8, 12, 16, 20, 24, 30, 36, 42, 50, 60, 72, 88, 106, 128, 156, 190, 230, 276, 330, 384, 576],
    [0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 54, 66, 82, 102, 126, 156, 194, 240, 296, 364, 448, 550, 576],
    [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
    [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
    [0, 12, 24, 36, 48, 60, 72, 88, 108, 132, 160, 192, 232, 280, 336, 400, 476, 566, 568, 570, 572, 574, 576],
];
pub(super) static SFB_SHORT: [[u16; 14]; 9] = [
    [0, 4, 8, 12, 18, 24, 32, 42, 56, 74, 100, 132, 174, 192],
    [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 136, 180, 192],
    [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 134, 174, 192],
    [0, 4, 8, 12, 16, 22, 30, 40, 52, 66, 84, 106, 136, 192],
    [0, 4, 8, 12, 16, 22, 28, 38, 50, 64, 80, 100, 126, 192],
    [0, 4, 8, 12, 16, 22, 30, 42, 58, 78, 104, 138, 180, 192],
    [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 134, 174, 192],
    [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 134, 174, 192],
    [0, 8, 16, 24, 36, 52, 72, 96, 124, 160, 162, 164, 166, 192],
];
pub(super) static PRETAB: [u8; 22] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 3, 2, 0];
pub(super) static SLEN1: [u8; 16] = [0, 0, 0, 0, 3, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4];
pub(super) static SLEN2: [u8; 16] = [0, 1, 2, 3, 0, 1, 2, 3, 1, 2, 3, 1, 2, 3, 2, 3];
// MPEG-2 LSF scale-factor partitions: [table][block kind: long, short, mixed][partition].
pub(super) static NR_OF_SFB: [[[u8; 4]; 3]; 6] = [
    [[6, 5, 5, 5], [9, 9, 9, 9], [6, 9, 9, 9]],
    [[6, 5, 7, 3], [9, 9, 12, 6], [6, 9, 12, 6]],
    [[11, 10, 0, 0], [18, 18, 0, 0], [15, 18, 0, 0]],
    [[7, 7, 7, 0], [12, 12, 12, 0], [6, 15, 12, 0]],
    [[6, 6, 6, 3], [12, 9, 9, 6], [6, 12, 9, 6]],
    [[8, 8, 5, 0], [15, 12, 9, 0], [6, 18, 9, 0]],
];

#[derive(Clone, Copy, Debug, Default)]
struct GrInfo {
    part23: usize,
    big_values: usize,
    global_gain: i32,
    scalefac_compress: usize,
    block_type: u8,
    switched: bool,
    mixed: bool,
    table_select: [u8; 3],
    subblock_gain: [i32; 3],
    region1_start: usize,
    region2_start: usize,
    preflag: bool,
    scalefac_scale: bool,
    count1_table: bool,
}
impl GrInfo {
    fn is_short(&self) -> bool { self.switched && self.block_type == 2 }
}

struct Bits<'a> { data: &'a [u8], pos: usize }
impl<'a> Bits<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self { Bits { data, pos } }
    fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
        let v = (u32::from(byte) >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        v
    }
    fn read(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n { v = (v << 1) | self.bit(); }
        v
    }
    fn overrun(&self) -> bool { self.pos > self.data.len() * 8 }
}

fn decode_symbol(tree: &Tree, bits: &mut Bits<'_>) -> Option<u16> {
    let mut at = 0usize;
    // The deepest standard code is 19 bits.
    for _ in 0..24 {
        let child = tree.nodes.get(at)?[bits.bit() as usize];
        if child == 0 { return None; }
        if child & 0x8000 != 0 { return Some(child & 0x7fff); }
        at = usize::from(child);
    }
    None
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 { crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 }; }
    }
    crc
}

/// Per-channel spectrum of one granule.
#[derive(Clone)]
struct ChannelSpec {
    gi: GrInfo,
    sf: [u8; 39],
    /// Largest transmittable scalefactor per partition (LSF intensity stereo).
    sf_max: [u8; 39],
    long: [f64; GRANULE],
    short: [[f64; 192]; 3],
    /// Number of leading long-spectrum lines that may be nonzero.
    nonzero: usize,
}
impl ChannelSpec {
    fn new() -> Box<ChannelSpec> {
        Box::new(ChannelSpec { gi: GrInfo::default(), sf: [0; 39], sf_max: [0; 39], long: [0.0; GRANULE], short: [[0.0; 192]; 3], nonzero: 0 })
    }
    fn clear(&mut self) {
        self.long = [0.0; GRANULE];
        self.short = [[0.0; 192]; 3];
        self.nonzero = 0;
    }
}

/// Polyphase synthesis state for one channel.
#[derive(Clone)]
struct Synth { v: [f64; 1024], pos: usize }
impl Synth {
    fn new() -> Synth { Synth { v: [0.0; 1024], pos: 0 } }
    /// One slot of 32 subband samples in, 32 PCM samples out.
    fn slot(&mut self, t: &Tables, s: &[f64; 32], out: &mut [f64]) {
        // T[m] = sum_k cos(m (2k+1) pi / 64) s[k], m = 0..31.
        let mut tm = [0.0f64; 32];
        for k in 0..32 {
            let sk = s[k];
            if sk == 0.0 { continue; }
            let col = &t.matrix;
            for m in 0..32 { tm[m] += col[m * 32 + k] * sk; }
        }
        self.pos = (self.pos + 1024 - 64) & 1023;
        let p = self.pos;
        // V[i] = T[16 + i]; T[32] = 0; T[64 - m] = -T[m]; T[64 + j] = -T[j].
        for i in 0..64usize {
            let m = 16 + i;
            let value = if m < 32 { tm[m] } else if m == 32 { 0.0 } else if m < 64 { -tm[64 - m] } else { -tm[m - 64] };
            self.v[(p + i) & 1023] = value;
        }
        let mut acc = [0.0f64; 32];
        for a in 0..8usize {
            let base_even = 128 * a;
            let base_odd = 128 * a + 96;
            let we = &t.window_even[a];
            let wo = &t.window_odd[a];
            for j in 0..32usize {
                acc[j] += self.v[(p + base_even + j) & 1023] * we[j];
                acc[j] += self.v[(p + base_odd + j) & 1023] * wo[j];
            }
        }
        out[..32].copy_from_slice(&acc);
    }
}

/// Decoded PCM of one MPEG frame, planar, unclipped, 1.0 = full scale.
pub struct Pcm { pub data: [[f64; 2 * GRANULE]; 2] }
impl Pcm { pub fn new() -> Box<Pcm> { Box::new(Pcm { data: [[0.0; 2 * GRANULE]; 2] }) } }

/// Why a granule could not be decoded (it then decodes as silence).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Damage {
    /// main_data_begin pointed before the bytes retained from earlier frames.
    ReservoirShort,
    /// The granule's declared length runs past the available main data.
    DataShort,
    Scalefactors,
    HuffmanTable,
    HuffmanCode,
    HuffmanOverrun,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStatus {
    pub granules: usize,
    /// Some granule had undecodable data and decoded as silence.
    pub damaged: bool,
    /// The first reason a granule was undecodable.
    pub damage: Option<Damage>,
    /// The frame's CRC field disagrees with its protected bytes.
    pub crc_mismatch: bool,
    /// main_data_begin pointed before the bytes retained from earlier frames.
    pub reservoir_short: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError { NotLayer3, Truncated }

#[derive(Clone)]
pub struct Decoder {
    reservoir: Vec<u8>,
    scratch: Vec<u8>,
    overlap: [[[f64; 18]; 32]; 2],
    synth: [Synth; 2],
    sf_prev: [[u8; 39]; 2],
    spec: [Box<ChannelSpec>; 2],
    ints: Box<[i32; GRANULE + 4]>,
    subband: Box<[[f64; 32]; 18]>,
}

impl Default for Decoder { fn default() -> Self { Self::new() } }

impl Decoder {
    pub fn new() -> Decoder {
        let _ = tables();
        Decoder {
            reservoir: Vec::with_capacity(MAX_MAIN_DATA + 1), scratch: Vec::new(),
            overlap: [[[0.0; 18]; 32]; 2], synth: [Synth::new(), Synth::new()], sf_prev: [[0; 39]; 2],
            spec: [ChannelSpec::new(), ChannelSpec::new()], ints: Box::new([0; GRANULE + 4]), subband: Box::new([[0.0; 32]; 18]),
        }
    }

    /// Decode one frame. `body` is everything after the 4 header bytes up to
    /// the end of the frame. The decoder history advances even when the frame
    /// is damaged.
    pub fn decode_frame(&mut self, hdr: &Header, body: &[u8], pcm: &mut Pcm) -> Result<FrameStatus, FrameError> {
        if hdr.layer != 3 { return Err(FrameError::NotLayer3); }
        let t = tables();
        let channels = hdr.channels();
        let lsf = hdr.version != Version::V1;
        let granules = if lsf { 1 } else { 2 };
        let si_len = hdr.side_info_bytes();
        let crc_len = if hdr.crc { 2 } else { 0 };
        if body.len() < crc_len + si_len { return Err(FrameError::Truncated); }
        let mut status = FrameStatus { granules, ..FrameStatus::default() };
        let side = &body[crc_len..crc_len + si_len];
        // A wrong CRC is reported, not corrected: AudioToolbox ignores the field, but when the bytes it
        // protects (the side information) are corrupt the two decoders produce different garbage.
        if hdr.crc {
            let mut protected = Vec::with_capacity(2 + si_len);
            protected.extend_from_slice(&hdr.raw.to_be_bytes()[2..]);
            protected.extend_from_slice(side);
            status.crc_mismatch = crc16(&protected) != u16::from_be_bytes([body[0], body[1]]);
        }

        // ---- side information
        let mut bits = Bits::new(side, 0);
        let main_data_begin = bits.read(if lsf { 8 } else { 9 }) as usize;
        bits.read(match (lsf, channels) { (false, 1) => 5, (false, _) => 3, (true, 1) => 1, (true, _) => 2 });
        let mut scfsi = [[false; 4]; 2];
        if !lsf {
            for ch in 0..channels { for g in 0..4 { scfsi[ch][g] = bits.bit() == 1; } }
        }
        let mut infos = [[GrInfo::default(); 2]; 2]; // [granule][channel]
        let band_set = hdr.band_set();
        for gr in 0..granules {
            for ch in 0..channels {
                let mut g = GrInfo::default();
                g.part23 = bits.read(12) as usize;
                g.big_values = (bits.read(9) as usize).min(288);
                g.global_gain = bits.read(8) as i32;
                g.scalefac_compress = bits.read(if lsf { 9 } else { 4 }) as usize;
                g.switched = bits.bit() == 1;
                if g.switched {
                    g.block_type = bits.read(2) as u8;
                    g.mixed = bits.bit() == 1;
                    g.table_select[0] = bits.read(5) as u8;
                    g.table_select[1] = bits.read(5) as u8;
                    for w in 0..3 { g.subblock_gain[w] = bits.read(3) as i32; }
                    if g.block_type == 0 { g.mixed = false; }
                    // Implicit region split (lines): the first region covers 36 lines
                    // for MPEG-1 and short blocks, 54 for other LSF block types, and
                    // twice those on the 8 kHz table; there is no third region.
                    g.region1_start = match (g.block_type == 2, hdr.version == Version::V1, band_set == 8) {
                        (true, _, false) | (_, true, _) => 36,
                        (true, _, true) => 72,
                        (false, false, false) => 54,
                        (false, false, true) => 108,
                    };
                    g.region2_start = GRANULE;
                } else {
                    for i in 0..3 { g.table_select[i] = bits.read(5) as u8; }
                    let r0 = bits.read(4) as usize;
                    let r1 = bits.read(3) as usize;
                    let long = &SFB_LONG[band_set];
                    g.region1_start = usize::from(long[(r0 + 1).min(22)]);
                    g.region2_start = usize::from(long[(r0 + r1 + 2).min(22)]);
                }
                if !lsf { g.preflag = bits.bit() == 1; }
                g.scalefac_scale = bits.bit() == 1;
                g.count1_table = bits.bit() == 1;
                infos[gr][ch] = g;
            }
        }

        // ---- main data: bit reservoir + this frame
        let main_new = &body[crc_len + si_len..];
        let have = self.reservoir.len();
        let mut available = true;
        self.scratch.clear();
        if main_data_begin > have {
            available = false;
            status.reservoir_short = true;
        } else {
            self.scratch.extend_from_slice(&self.reservoir[have - main_data_begin..]);
            self.scratch.extend_from_slice(main_new);
        }
        self.reservoir.extend_from_slice(main_new);
        if self.reservoir.len() > MAX_MAIN_DATA {
            let drop = self.reservoir.len() - MAX_MAIN_DATA;
            self.reservoir.drain(..drop);
        }
        let data = std::mem::take(&mut self.scratch);

        // ---- granules
        let ms_stereo = hdr.mode == 1 && hdr.mode_ext & 2 != 0;
        let is_stereo = hdr.mode == 1 && hdr.mode_ext & 1 != 0;
        let mut offset = 0usize;
        for gr in 0..granules {
            for ch in 0..channels {
                let g = infos[gr][ch];
                self.spec[ch].clear();
                self.spec[ch].gi = g;
                let result = if !available {
                    Err(Damage::ReservoirShort)
                } else if !offset.checked_add(g.part23).is_some_and(|end| end <= data.len() * 8) {
                    Err(Damage::DataShort)
                } else {
                    let is_right_with_intensity = lsf && is_stereo && ch == 1;
                    let scf = if gr == 1 { scfsi[ch] } else { [false; 4] };
                    self.decode_channel(hdr, &data, offset, scf, ch, is_right_with_intensity, t, band_set)
                };
                if let Err(kind) = result {
                    self.spec[ch].clear();
                    self.spec[ch].gi = g;
                    status.damaged = true;
                    status.damage.get_or_insert(kind);
                }
                offset = offset.saturating_add(g.part23);
            }
            if channels == 2 {
                self.stereo(hdr, ms_stereo, is_stereo, t, band_set);
            }
            for ch in 0..channels {
                self.synthesize(ch, gr, t, pcm);
            }
        }
        self.scratch = data;
        Ok(status)
    }

    /// Scale factors, Huffman data and requantization of one granule/channel.
    #[allow(clippy::too_many_arguments)]
    fn decode_channel(&mut self, hdr: &Header, data: &[u8], start: usize, scfsi: [bool; 4], ch: usize,
                      intensity_right: bool, t: &Tables, band_set: usize) -> Result<(), Damage> {
        let g = self.spec[ch].gi;
        let end = start + g.part23;
        let mut bits = Bits::new(data, start);
        let lsf = hdr.version != Version::V1;
        let mut sf = [0u8; 39];
        let mut sf_max = [0u8; 39];
        let mut g = g;
        if lsf {
            if !self.read_scalefactors_lsf(&mut bits, &mut g, &mut sf, &mut sf_max, intensity_right) { return Err(Damage::Scalefactors); }
        } else {
            self.read_scalefactors_v1(&mut bits, &g, scfsi, ch, &mut sf);
        }
        self.sf_prev[ch] = sf;
        if bits.pos > end || bits.overrun() { return Err(Damage::Scalefactors); }
        self.spec[ch].gi = g;
        self.spec[ch].sf = sf;
        self.spec[ch].sf_max = sf_max;

        // ---- Huffman
        let ints = &mut *self.ints;
        ints.fill(0);
        let mut line = 0usize;
        let big_end = (g.big_values * 2).min(GRANULE);
        while line < big_end {
            let region = if line < g.region1_start { 0 } else if line < g.region2_start { 1 } else { 2 };
            let table_no = usize::from(g.table_select[region]);
            let Some(table) = super::mp3_tables::BIG_TABLES.get(table_no).and_then(|t| t.as_ref()) else {
                if table_no == 0 { line += 2; continue; }
                return Err(Damage::HuffmanTable);
            };
            let Some(tree) = t.big_trees.get(table_no).and_then(|t| t.as_ref()) else { return Err(Damage::HuffmanTable); };
            let Some(symbol) = decode_symbol(tree, &mut bits) else { return Err(Damage::HuffmanCode); };
            let dim = usize::from(table.dim);
            let mut x = i32::from(symbol) / dim as i32;
            let mut y = i32::from(symbol) % dim as i32;
            let linbits = u32::from(super::mp3_tables::LINBITS[table_no]);
            if linbits > 0 && x == 15 { x += bits.read(linbits) as i32; }
            if x != 0 && bits.bit() == 1 { x = -x; }
            if linbits > 0 && y == 15 { y += bits.read(linbits) as i32; }
            if y != 0 && bits.bit() == 1 { y = -y; }
            ints[line] = x;
            ints[line + 1] = y;
            line += 2;
            if bits.pos > end { return Err(Damage::HuffmanOverrun); }
        }
        let tree = &t.count1_trees[usize::from(g.count1_table)];
        while line + 4 <= GRANULE && bits.pos < end {
            let Some(symbol) = decode_symbol(tree, &mut bits) else { return Err(Damage::HuffmanCode); };
            let mut quad = [0i32; 4];
            for (i, slot) in quad.iter_mut().enumerate() {
                if symbol >> (3 - i) & 1 != 0 { *slot = if bits.bit() == 1 { -1 } else { 1 }; }
            }
            if bits.pos > end { break; } // the final quad ran past the granule: discard it
            ints[line..line + 4].copy_from_slice(&quad);
            line += 4;
        }
        if bits.overrun() { return Err(Damage::HuffmanOverrun); }
        let nonzero = ints[..GRANULE].iter().rposition(|&v| v != 0).map_or(0, |p| p + 1);
        self.spec[ch].nonzero = nonzero;

        // ---- requantization
        self.requantize(ch, t, band_set);
        Ok(())
    }

    fn read_scalefactors_v1(&self, bits: &mut Bits<'_>, g: &GrInfo, scfsi: [bool; 4], ch: usize, sf: &mut [u8; 39]) {
        let s1 = u32::from(SLEN1[g.scalefac_compress & 15]);
        let s2 = u32::from(SLEN2[g.scalefac_compress & 15]);
        if g.is_short() {
            if g.mixed {
                for i in 0..8 { sf[i] = bits.read(s1) as u8; }
                for i in 3..6 { for w in 0..3 { sf[8 + (i - 3) * 3 + w] = bits.read(s1) as u8; } }
                for i in 6..12 { for w in 0..3 { sf[17 + (i - 6) * 3 + w] = bits.read(s2) as u8; } }
            } else {
                for i in 0..6 { for w in 0..3 { sf[i * 3 + w] = bits.read(s1) as u8; } }
                for i in 6..12 { for w in 0..3 { sf[i * 3 + w] = bits.read(s2) as u8; } }
            }
        } else {
            let groups: [(usize, usize, u32); 4] = [(0, 6, s1), (6, 11, s1), (11, 16, s2), (16, 21, s2)];
            for (gi, &(from, to, len)) in groups.iter().enumerate() {
                if scfsi[gi] {
                    sf[from..to].copy_from_slice(&self.sf_prev[ch][from..to]);
                } else {
                    for slot in &mut sf[from..to] { *slot = bits.read(len) as u8; }
                }
            }
        }
    }

    fn read_scalefactors_lsf(&self, bits: &mut Bits<'_>, g: &mut GrInfo, sf: &mut [u8; 39], sf_max: &mut [u8; 39], intensity_right: bool) -> bool {
        let mut sc = g.scalefac_compress;
        let slen: [u32; 4];
        let table;
        if intensity_right {
            sc >>= 1;
            if sc < 180 {
                slen = [(sc / 36) as u32, ((sc % 36) / 6) as u32, ((sc % 36) % 6) as u32, 0];
                table = 3;
            } else if sc < 244 {
                let s = sc - 180;
                slen = [((s & 63) >> 4) as u32, ((s & 15) >> 2) as u32, (s & 3) as u32, 0];
                table = 4;
            } else {
                let s = sc - 244;
                slen = [(s / 3) as u32, (s % 3) as u32, 0, 0];
                table = 5;
            }
        } else if sc < 400 {
            slen = [((sc >> 4) / 5) as u32, ((sc >> 4) % 5) as u32, ((sc & 15) >> 2) as u32, (sc & 3) as u32];
            table = 0;
        } else if sc < 500 {
            let s = sc - 400;
            slen = [((s >> 2) / 5) as u32, ((s >> 2) % 5) as u32, (s & 3) as u32, 0];
            table = 1;
        } else {
            let s = sc - 500;
            slen = [(s / 3) as u32, (s % 3) as u32, 0, 0];
            table = 2;
            g.preflag = true;
        }
        let kind = if g.is_short() { if g.mixed { 2 } else { 1 } } else { 0 };
        let counts = NR_OF_SFB[table][kind];
        let mut at = 0usize;
        for part in 0..4 {
            let n = usize::from(counts[part]);
            for _ in 0..n {
                if at >= 39 { return false; }
                sf[at] = if slen[part] == 0 { 0 } else { bits.read(slen[part]) as u8 };
                sf_max[at] = ((1u32 << slen[part]) - 1) as u8;
                at += 1;
            }
        }
        true
    }

    fn requantize(&mut self, ch: usize, t: &Tables, band_set: usize) {
        let g = self.spec[ch].gi;
        let sf = self.spec[ch].sf;
        let ints = &*self.ints;
        let long_edges = &SFB_LONG[band_set];
        let short_edges = &SFB_SHORT[band_set];
        // scalefac_scale: multiplier 0.5 -> 2 quarter steps per unit, 1.0 -> 4.
        let sf_quarters = if g.scalefac_scale { 4 } else { 2 };
        let gain = g.global_gain - 210;
        let q = |e: i32| -> f64 {
            let idx = (e + QUARTER_BIAS).clamp(0, t.quarter.len() as i32 - 1);
            t.quarter[idx as usize]
        };
        let value = |i: i32, factor: f64| -> f64 {
            let a = i.unsigned_abs().min(8206) as usize;
            let m = t.pow43[a] * factor;
            if i < 0 { -m } else { m }
        };
        let spec = &mut *self.spec[ch];
        if !g.is_short() {
            for sfb in 0..22usize {
                let from = usize::from(long_edges[sfb]);
                let to = usize::from(long_edges[sfb + 1]);
                let pre = if g.preflag { i32::from(PRETAB[sfb]) } else { 0 };
                let e = gain - sf_quarters * (i32::from(sf[sfb]) + pre);
                let f = q(e);
                for line in from..to { spec.long[line] = value(ints[line], f); }
            }
        } else {
            let mut at = 0usize;
            let first_short = if g.mixed {
                let long_bands = if band_set < 3 || band_set > 5 { 6 } else { 8 };
                for sfb in 0..long_bands {
                    let from = usize::from(long_edges[sfb]);
                    let to = usize::from(long_edges[sfb + 1]);
                    let e = gain - sf_quarters * i32::from(sf[sfb]);
                    let f = q(e);
                    for line in from..to { spec.long[line] = value(ints[line], f); }
                    at = to;
                }
                3
            } else { 0 };
            for sfb in first_short..13 {
                let width = usize::from(short_edges[sfb + 1] - short_edges[sfb]);
                let start = usize::from(short_edges[sfb]);
                for w in 0..3usize {
                    let idx = if g.mixed { if sfb < 6 { 8 + (sfb - 3) * 3 + w } else { 17 + (sfb - 6) * 3 + w } } else { sfb * 3 + w };
                    let s = if sfb < 12 { i32::from(sf[idx.min(38)]) } else { 0 };
                    let e = gain - 8 * g.subblock_gain[w] - sf_quarters * s;
                    let f = q(e);
                    for k in 0..width {
                        spec.short[w][start + k] = value(ints[at.min(GRANULE - 1)], f);
                        at += 1;
                    }
                }
            }
        }
    }

    /// Mid/side and intensity processing of the two channels' spectra.
    fn stereo(&mut self, hdr: &Header, ms: bool, intensity: bool, t: &Tables, band_set: usize) {
        let lsf = hdr.version != Version::V1;
        let (left, right) = self.spec.split_at_mut(1);
        let l = &mut *left[0];
        let r = &mut *right[0];
        let short = r.gi.is_short();
        let long_edges = &SFB_LONG[band_set];
        let short_edges = &SFB_SHORT[band_set];
        let mut coded_long = [false; GRANULE];
        let mut coded_short = [[false; 192]; 3];
        if intensity {
            // Position index of a band; None marks the "illegal" value that keeps
            // the band as ordinary left/right (or mid/side) data.
            let position = |sf_index: usize| -> Option<usize> {
                let index = sf_index.min(38);
                let pos = usize::from(r.sf[index]);
                if lsf {
                    if r.sf_max[index] != 0 && pos == usize::from(r.sf_max[index]) { None } else { Some(pos) }
                } else if pos >= 7 { None } else { Some(pos) }
            };
            // LSF attenuation per half step: 2^(-1/4) or 2^(-1/2) per two positions.
            let io = if r.gi.scalefac_compress & 1 == 1 { t.quarter[(QUARTER_BIAS - 2) as usize] } else { t.quarter[(QUARTER_BIAS - 1) as usize] };
            let apply = |a: &mut f64, b: &mut f64, pos: usize| {
                let v = *a;
                if lsf {
                    if pos == 0 { *b = v; return; }
                    let mut p = 1.0f64;
                    for _ in 0..(pos + 1) / 2 { p *= io; }
                    if pos % 2 == 1 { *a = v * p; *b = v; } else { *a = v; *b = v * p; }
                } else {
                    *a = v * t.is_left[pos.min(6)];
                    *b = v * t.is_right[pos.min(6)];
                }
            };
            if !short {
                // Bands above the last one with right-channel data are intensity coded.
                let mut bound = 0usize;
                for sfb in 0..22 {
                    if r.long[usize::from(long_edges[sfb])..usize::from(long_edges[sfb + 1])].iter().any(|&v| v != 0.0) { bound = sfb + 1; }
                }
                for sfb in bound..22 {
                    // The last band has no scale factor of its own: it reuses band 20's.
                    let pos = position(sfb.min(20));
                    if let Some(pos) = pos {
                        for line in usize::from(long_edges[sfb])..usize::from(long_edges[sfb + 1]) {
                            apply(&mut l.long[line], &mut r.long[line], pos);
                            coded_long[line] = true;
                        }
                    }
                }
            } else {
                let long_count = if band_set >= 3 && band_set <= 5 { 8 } else { 6 };
                let first = if r.gi.mixed { 3 } else { 0 };
                for w in 0..3usize {
                    let sf_index = |sfb: usize| if r.gi.mixed { long_count + (sfb - 3) * 3 + w } else { sfb * 3 + w };
                    let mut bound = first;
                    for sfb in first..13 {
                        if r.short[w][usize::from(short_edges[sfb])..usize::from(short_edges[sfb + 1])].iter().any(|&v| v != 0.0) { bound = sfb + 1; }
                    }
                    for sfb in bound..13 {
                        let pos = position(sf_index(sfb.min(11)));
                        if let Some(pos) = pos {
                            for f in usize::from(short_edges[sfb])..usize::from(short_edges[sfb + 1]) {
                                apply(&mut l.short[w][f], &mut r.short[w][f], pos);
                                coded_short[w][f] = true;
                            }
                        }
                    }
                }
            }
        }
        if ms {
            for line in 0..GRANULE {
                if coded_long[line] { continue; }
                let (m, s) = (l.long[line], r.long[line]);
                l.long[line] = (m + s) * SQRT_HALF;
                r.long[line] = (m - s) * SQRT_HALF;
            }
            for w in 0..3 {
                for f in 0..192 {
                    if coded_short[w][f] { continue; }
                    let (m, s) = (l.short[w][f], r.short[w][f]);
                    l.short[w][f] = (m + s) * SQRT_HALF;
                    r.short[w][f] = (m - s) * SQRT_HALF;
                }
            }
        }
        // Both channels now share one nonzero extent.
        if ms || intensity {
            let n = l.nonzero.max(r.nonzero);
            l.nonzero = n;
            r.nonzero = n;
        }
    }

    /// Alias reduction, IMDCT, overlap-add and polyphase synthesis for one
    /// granule of one channel; writes 576 PCM samples.
    fn synthesize(&mut self, ch: usize, gr: usize, t: &Tables, pcm: &mut Pcm) {
        let spec = &mut *self.spec[ch];
        let g = spec.gi;
        let short = g.is_short();
        let active_sb = if short { 32 } else { ((spec.nonzero + 17) / 18).min(32) };
        // Alias reduction between adjacent long subbands.
        let boundaries = if short { usize::from(g.mixed) } else { active_sb.min(31) };
        for b in 1..=boundaries {
            for i in 0..8usize {
                let lo = spec.long[18 * b - 1 - i];
                let hi = spec.long[18 * b + i];
                spec.long[18 * b - 1 - i] = lo * t.alias_cs[i] - hi * t.alias_ca[i];
                spec.long[18 * b + i] = hi * t.alias_cs[i] + lo * t.alias_ca[i];
            }
        }
        let process_sb = if short { 32 } else { (active_sb + 1).min(32) };
        let sub = &mut *self.subband;
        for sb in 0..32usize {
            let mut y = [0.0f64; 36];
            if sb < process_sb {
                if !short || (g.mixed && sb < 2) {
                    let x = &spec.long[18 * sb..18 * sb + 18];
                    if x.iter().any(|&v| v != 0.0) {
                        let bt = if short { 0 } else { usize::from(g.block_type) };
                        let basis = &t.imdct36[bt];
                        for (k, &xv) in x.iter().enumerate() {
                            if xv == 0.0 { continue; }
                            let row = &basis[k * 36..k * 36 + 36];
                            for i in 0..36 { y[i] += xv * row[i]; }
                        }
                    }
                } else {
                    for w in 0..3usize {
                        let x = &spec.short[w][6 * sb..6 * sb + 6];
                        if x.iter().all(|&v| v == 0.0) { continue; }
                        let mut z = [0.0f64; 12];
                        for (k, &xv) in x.iter().enumerate() {
                            if xv == 0.0 { continue; }
                            let row = &t.imdct12[k * 12..k * 12 + 12];
                            for i in 0..12 { z[i] += xv * row[i]; }
                        }
                        for i in 0..12 { y[6 + 6 * w + i] += z[i]; }
                    }
                }
            }
            let ov = &mut self.overlap[ch][sb];
            for i in 0..18usize {
                let v = y[i] + ov[i];
                ov[i] = y[18 + i];
                // Frequency inversion of odd subbands at odd time slots.
                sub[i][sb] = if sb % 2 == 1 && i % 2 == 1 { -v } else { v };
            }
        }
        for slot in 0..18usize {
            let row = sub[slot];
            let out = &mut pcm.data[ch][gr * GRANULE + slot * 32..gr * GRANULE + slot * 32 + 32];
            self.synth[ch].slot(t, &row, out);
        }
    }
}
