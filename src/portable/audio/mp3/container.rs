//! Exact model of Apple's MP3 container layer as seen by the engine's `apple_codec::Reader`
//! (`ExtAudioFileOpenURL` -> `AudioFileOpenURL` -> `MPEGAudioFile::OpenFromDataSource` /
//! `ScanForPackets` / `GetMetaHeader` / `ParseVBRIPacket`, plus the ExtAudioFile length / priming rules).
//!
//! Everything here is a transcription of AudioToolboxCore (macOS 27.2, arm64e dyld shared cache) behaviour; the
//! function names in the comments are the symbols of that binary. Decoder independent: it produces the packet
//! list that `ExtAudioFile` feeds to the codec, the source format, the packet table info (priming / remainder /
//! valid frames), the file length and the bitrate property. `super::gapless` cuts the decoder's PCM exactly like
//! `ExtAudioFileRead` does and models the read-level error rules.
//!
//! No `unsafe`, std only.

use std::fmt;

/// `.mp1` / `.mp2` / `.mp3` format ids (`ParseMPEGSyncWord`: 0x2e6d7031 + (layer - 1)).
pub const FORMAT_MP1: u32 = 0x2e6d_7031;
pub const FORMAT_MP2: u32 = 0x2e6d_7032;
pub const FORMAT_MP3: u32 = 0x2e6d_7033;

/// OSStatus values returned by the container layer (as seen through `ExtAudioFileOpenURL`).
pub const STATUS_DAT: i32 = 0x6474_613f; // 'dta?' kAudioFileInvalidFileError (the AudioFile "data refused" status)
pub const STATUS_NO_SYNC: i32 = -39; // 0xffffffd9 eofErr: no sync word / end of data
pub const STATUS_WHT: i32 = 0x7768_743f; // 'wht?' kAudioFileUnspecifiedError
pub const STATUS_FMT: i32 = 0x666d_743f; // 'fmt?' kAudioFormatUnsupportedDataFormatError (converter creation)

/// Result of `ParseMPEGSyncWord` (the `MPEGPacketDesc` struct, fields at the offsets noted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameDesc {
    /// +0 the header word.
    pub word: u32,
    /// +4 version code: 1 = MPEG-1, 2 = MPEG-2, 3 = MPEG-2.5.
    pub version: u8,
    /// +5 layer code: 1..=3 (Layer I..III).
    pub layer: u8,
    /// +8 frame size in bytes including the padding slot.
    pub size: u32,
    /// +0xc frame size of bitrate index 14 (+padding) for this version/layer/rate.
    pub max_size: u32,
    /// +0x10 bit rate in bit/s (kbps * 1000).
    pub bitrate_bps: u32,
    /// +0x14 bit rate in kbps.
    pub kbps: u32,
    /// +0x18 sample rate (stored as f64 by Apple).
    pub sample_rate: u32,
    /// +0x20 `.mp1` / `.mp2` / `.mp3`.
    pub format_id: u32,
    /// +0x2c frames per packet (384 / 1152 / 576).
    pub frames_per_packet: u32,
    /// +0x34 channels (1 for mode 3, else 2).
    pub channels: u32,
}

/// kbps tables selected by (version code, layer code): `ParseMPEGSyncWord` tables at 0x19eba0c98.. in the binary.
const KBPS: [[u32; 15]; 5] = [
    [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448], // MPEG-1 L1
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],    // MPEG-1 L2
    [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],     // MPEG-1 L3
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],    // MPEG-2/2.5 L1
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],         // MPEG-2/2.5 L2/L3
];

/// `ParseMPEGSyncWord(word, desc)`: 11 sync bits, version != reserved, layer != reserved, bitrate index 1..=14,
/// sample-rate index != 3. Emphasis, mode extension, protection, private/copyright/original bits are not checked.
pub fn parse_sync_word(w: u32) -> Option<FrameDesc> {
    if w < 0xffe0_0000 {
        return None;
    }
    let version = match (w >> 19) & 3 {
        0 => 3u8,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let layer = match (w >> 17) & 3 {
        1 => 3u8,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let bidx = ((w >> 12) & 15) as usize;
    if bidx == 15 {
        return None;
    }
    let group = match (version, layer) {
        (1, 1) => 0,
        (1, 2) => 1,
        (1, 3) => 2,
        (_, 1) => 3,
        _ => 4,
    };
    let kbps = KBPS[group][bidx];
    if kbps == 0 {
        return None;
    }
    let sidx = ((w >> 10) & 3) as usize;
    if sidx == 3 {
        return None;
    }
    let rate = match version {
        1 => [44100, 48000, 32000],
        2 => [22050, 24000, 16000],
        _ => [11025, 12000, 8000],
    }[sidx];
    let channels = if w & 0xc0 == 0xc0 { 1 } else { 2 };
    let kbps_max = KBPS[group][14];
    let (format_id, fpp, coef) = match layer {
        3 => {
            let fpp = if rate > 24000 { 1152 } else { 576 };
            (FORMAT_MP3, fpp, fpp * 125)
        }
        2 => (FORMAT_MP2, 1152, 144000),
        _ => (FORMAT_MP1, 384, 48000),
    };
    let mut size = coef * kbps / rate;
    let mut max_size = coef * kbps_max / rate;
    let pad = w & 0x200 != 0;
    if layer == 1 {
        size &= 0xfffc;
        max_size &= 0xfffc;
        if pad {
            size += 4;
            max_size += 4;
        }
    } else if pad {
        size += 1;
        max_size += 1;
    }
    Some(FrameDesc {
        word: w,
        version,
        layer,
        size,
        max_size,
        bitrate_bps: kbps * 1000,
        kbps,
        sample_rate: rate,
        format_id,
        frames_per_packet: fpp,
        channels,
    })
}

/// `MPEGPacketParser(data, len)`: valid iff at least 4 bytes and a valid sync word.
fn parse_at(data: &[u8], pos: u64) -> Option<FrameDesc> {
    let p = usize::try_from(pos).ok()?;
    let b = data.get(p..p.checked_add(4)?)?;
    parse_sync_word(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// `ID3v1TagExistsDS`: file >= 128 bytes and "TAG" at size-128.
fn id3v1_exists(data: &[u8]) -> bool {
    data.len() >= 128 && &data[data.len() - 128..data.len() - 125] == b"TAG"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncErr {
    /// -39: no (acceptable) sync word before the end of the data.
    NoSync,
    /// 'dta?': ten consecutive non-matching candidates.
    Dat,
}

struct Found {
    pos: u64,
    desc: FrameDesc,
}

/// `GetFPosForNextSyncWordDS(ds, &id3v1Flag, start, end, &pos, &ver, &layer, &size, &rate, &channels)`.
/// Scans for the next valid header (first byte 0xff + a valid `ParseMPEGSyncWord`) in 0x403-byte windows that
/// overlap by 3 bytes. With an ID3v1 tag the search area ends 128 bytes early and a frame running into the tag is
/// not accepted.
fn next_sync(data: &[u8], id3v1: bool, start: i64, end: i64) -> Result<Found, SyncErr> {
    let mut start = start;
    if start >= end {
        return Err(SyncErr::NoSync);
    }
    let data_end = end - 128;
    while start < end {
        if id3v1 && start >= data_end {
            break;
        }
        let n = if start + 0x403 <= end { 0x403 } else { end - start };
        if n < 4 {
            break;
        }
        let s = start as usize;
        let e = (s + n as usize).min(data.len());
        let got = e.saturating_sub(s);
        if got == 0 {
            return Err(SyncErr::NoSync);
        }
        if got < 4 {
            // Apple's loop makes no progress here (the file shrank under it); treat as end of data.
            return Err(SyncErr::NoSync);
        }
        let buf = &data[s..e];
        for i in 0..got - 3 {
            if buf[i] != 0xff {
                continue;
            }
            let w = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
            if let Some(desc) = parse_sync_word(w) {
                let pos = start + i as i64;
                if id3v1 && data_end < pos + i64::from(desc.size) {
                    return Err(SyncErr::NoSync);
                }
                return Ok(Found { pos: pos as u64, desc });
            }
        }
        start += (got - 3) as i64;
    }
    Err(SyncErr::NoSync)
}

/// `GetFPosForNextMatchingSyncWordDS`: up to ten candidates; a candidate matches when version, layer, sample rate
/// and channel count equal the stream's. A non-matching candidate restarts the search one byte after it. Besides the
/// status it returns the out parameter, which is written with the position of every candidate found (so after a failure it
/// holds the last non-matching candidate; `start` when none was found).
fn next_matching_sync_out(data: &[u8], id3v1: bool, fmt: &Fmt, start: i64, end: i64) -> (Result<u64, SyncErr>, i64) {
    let mut start = start;
    let mut out = start;
    for _ in 0..10 {
        let f = match next_sync(data, id3v1, start, end) {
            Ok(f) => f,
            Err(e) => return (Err(e), out),
        };
        out = f.pos as i64;
        let d = f.desc;
        if d.version == fmt.version && d.layer == fmt.layer && d.channels == fmt.channels && d.sample_rate == fmt.sample_rate {
            return (Ok(f.pos), out);
        }
        start = f.pos as i64 + 1;
    }
    (Err(SyncErr::Dat), out)
}

fn next_matching_sync(data: &[u8], id3v1: bool, fmt: &Fmt, start: i64, end: i64) -> Result<u64, SyncErr> {
    next_matching_sync_out(data, id3v1, fmt, start, end).0
}

/// Stream parameters fixed by the first accepted frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fmt {
    version: u8,
    layer: u8,
    sample_rate: u32,
    channels: u32,
    fpp: u32,
    format_id: u32,
}

/// Why `ExtAudioFileOpenURL` (the container layer) refuses a file, with the OSStatus Apple returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// 'dta?' (kAudioFileInvalidFileError): no stream start accepted, invalid first packet, ...
    Dat,
    /// -39 (eofErr): the Xing/VBRI/Info frame is not followed by a matching audio frame.
    Eof,
    /// 'wht?' (kAudioFileUnspecifiedError): empty file, or the file consists only of ID3 tag blocks (`ID3Size` chain).
    Wht,
    /// Apple never returns (a chained "ID3" header whose version or revision byte is 0xff; observed: the open call spins
    /// until killed). Refused here with 'dta?'.
    Hang,
}

impl Refusal {
    pub fn os_status(self) -> i32 {
        match self {
            Refusal::Dat | Refusal::Hang => STATUS_DAT,
            Refusal::Eof => STATUS_NO_SYNC,
            Refusal::Wht => STATUS_WHT,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Dat => f.write_str("AudioFile refuses the data (dta?)"),
            Refusal::Eof => f.write_str("no audio frame after the tag frame (eofErr)"),
            Refusal::Wht => f.write_str("AudioFile cannot identify the file (wht?)"),
            Refusal::Hang => f.write_str("ID3v2 chain does not terminate"),
        }
    }
}

impl std::error::Error for Refusal {}

impl SyncErr {
    /// Status Apple returns when the matching-sync search after the tag frame fails.
    fn refusal(self) -> Refusal {
        match self {
            SyncErr::NoSync => Refusal::Eof,
            SyncErr::Dat => Refusal::Dat,
        }
    }
}

/// `ID3Size(len, hdr, &type)`: size of a leading ID3v2 ("ID3" + version/revision != 0xff, syncsafe size + 10, no footer) or
/// ID3v1 ("TAG": 128) block, 0 otherwise.
pub fn id3_size(h: &[u8]) -> i64 {
    if h.len() > 9 && &h[..3] == b"ID3" && h[3] != 0xff && h[4] != 0xff {
        return i64::from(((u32::from(h[6]) & 0x7f) << 21) | ((u32::from(h[7]) & 0x7f) << 14) | ((u32::from(h[8]) & 0x7f) << 7) | (u32::from(h[9]) & 0x7f)) + 10;
    }
    if h.len() > 2 && &h[..3] == b"TAG" {
        return 128;
    }
    0
}

/// The generic `AudioFile` layer skips leading ID3/TAG blocks before it tries the format openers; when nothing is left it
/// answers 'wht?'.
fn generic_open_check(data: &[u8]) -> Result<(), Refusal> {
    if data.is_empty() {
        return Err(Refusal::Wht);
    }
    let mut off: i64 = 0;
    loop {
        let o = off as usize;
        if o >= data.len() {
            break;
        }
        let sz = id3_size(&data[o..data.len().min(o + 10)]);
        if sz == 0 {
            return Ok(());
        }
        off += sz;
    }
    if off > 0 { Err(Refusal::Wht) } else { Ok(()) }
}

/// `AudioFilePacketTableInfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketTableInfo {
    pub valid_frames: i64,
    pub priming: i32,
    pub remainder: i32,
}

/// Decoded `XingHeader` (GetMetaHeader). `flags`: 1 frames, 2 bytes, 4 TOC, 8 quality, 0x10 "Info" tag,
/// 0x20 LAME tag with delay/padding accepted, 0x40 replay-gain data present.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Xing {
    pub flags: u32,
    pub frames: u32,
    pub bytes: u32,
    pub lame_id: u32,
    pub delay: u16,
    pub padding: u16,
}

fn be32(p: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]])
}

fn crc16_arc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        let mut x = (crc ^ u32::from(b)) & 0xff;
        for _ in 0..8 {
            x = if x & 1 != 0 { (x >> 1) ^ 0xa001 } else { x >> 1 };
        }
        crc = x ^ (crc >> 8);
    }
    crc
}

/// `GetMetaHeader(XingHeader&, frame, size)`: `Some` iff the first frame carries a Xing/Info header at the fixed
/// offset (MPEG-1: 36 stereo / 21 mono; MPEG-2/2.5: 21 stereo / 13 mono; the CRC protection bit is ignored).
pub fn get_meta_header(p: &[u8]) -> Option<Xing> {
    let n = p.len();
    if n < 4 {
        return None;
    }
    let mono = p[3] >= 0xc0;
    let mpeg1 = (p[1] >> 3) & 1 != 0;
    let off = match (mpeg1, mono) {
        (true, true) => 0x15,
        (true, false) => 0x24,
        (false, true) => 0xd,
        (false, false) => 0x15,
    };
    if n < off + 4 {
        return None;
    }
    let tag = be32(p, off);
    if tag != 0x5869_6e67 && tag != 0x496e_666f {
        return None;
    }
    let info = tag == 0x496e_666f;
    let mut x = Xing { flags: if info { 0x10 } else { 0 }, ..Default::default() };
    if n < off + 8 {
        return None;
    }
    let fl = p[off + 7];
    let mut cur = off + 8;
    if fl & 1 != 0 {
        if n < off + 12 {
            return None;
        }
        x.flags |= 1;
        x.frames = be32(p, cur);
        cur = off + 12;
    }
    if fl & 2 != 0 {
        if n < cur + 4 {
            return None;
        }
        x.flags |= 2;
        x.bytes = be32(p, cur);
        cur += 4;
    }
    if fl & 4 != 0 {
        if n < cur + 100 {
            return None;
        }
        x.flags |= 4;
        cur += 100;
    }
    if fl & 8 != 0 {
        if n < cur + 4 {
            return None;
        }
        x.flags |= 8;
        cur += 4;
    }
    if cur + 0x24 <= n {
        let stored = (u32::from(p[cur + 0x22]) << 8) | u32::from(p[cur + 0x23]);
        x.lame_id = be32(p, cur);
        if crc16_arc(&p[..cur + 0x22]) == stored || x.lame_id == 0x4c41_4d45 {
            let peak = be32(p, cur + 0xb);
            let radio = p[cur + 0xf];
            let audiophile = p[cur + 0x11];
            if peak != 0 || audiophile > 3 || radio > 3 {
                x.flags |= 0x40;
            }
            x.flags |= 0x20;
            x.delay = (u16::from(p[cur + 0x15]) << 4) | u16::from(p[cur + 0x16] >> 4);
            x.padding = u16::from(p[cur + 0x17]) | (u16::from(p[cur + 0x16] & 0xf) << 8);
        }
    }
    if info && (x.lame_id == 0x4c41_4d45 || x.lame_id == 0) {
        x.flags &= 0x7e;
    }
    Some(x)
}

/// `ParseVBRIPacket` (Fraunhofer VBRI header at frame offset 36): `(frames, bytes, quality_or_delay)`.
fn parse_vbri(p: &[u8]) -> Option<(u32, u32, u16)> {
    // Layout (big endian): +36 "VBRI", +40 version u16, +42 delay u16, +44 quality u16, +46 bytes u32,
    // +50 frames u32, +54 toc entries u16, +56 scale u16, +58 entry size u16, +60 frames per entry u16.
    if p.len() < 36 + 18 || &p[36..40] != b"VBRI" {
        return None;
    }
    let quality = u16::from_be_bytes([p[44], p[45]]);
    let bytes = be32(p, 46);
    let frames = be32(p, 50);
    Some((frames, bytes, quality))
}

/// One MPEG frame handed to the codec as an AudioFile packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Absolute file offset of the frame header.
    pub offset: u64,
    /// Frame size in bytes (from the header).
    pub size: u32,
    /// Header bit rate in kbps (summed by `ScanForPackets` for the bit rate property).
    pub kbps: u32,
}

/// Source stream description (`kExtAudioFileProperty_FileDataFormat`): flags 0, bytes per packet 0, bytes per frame
/// 0, bits per channel 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFormat {
    pub sample_rate: u32,
    pub format_id: u32,
    pub frames_per_packet: u32,
    pub channels: u32,
    pub version: u8,
    pub layer: u8,
}

/// Result of `MPEGAudioFile::ScanForPackets(-1)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scan {
    /// The table as Apple builds it.
    Complete(Vec<Packet>),
    /// `ScanForPackets` failed ('dta?') while looking for the successor of packet `at` (< 10): the table is deleted
    /// and every group read that needs the table to reach packet `at` fails; the `at` packets found before are readable.
    Failed { at: usize, readable: Vec<Packet> },
}

/// How the packet scan of a file ends (what `MPEGAudioFile::ScanForPackets` does once it has appended every packet it can).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanEnd {
    /// The successor search of the last packet found no sync word (-39): the packet is appended and the scan stops.
    NoSync,
    /// Ten failed candidates after at least ten packets: treated like [`ScanEnd::NoSync`] for the table, but a later
    /// scan call that starts over from the last packet fails with `'dta?'` unless an earlier call already marked the
    /// table complete.
    Fail10,
    /// The next packet would cross the end of the data: it is not appended.
    CrossEnd,
    /// The header at the next position is invalid (never seen: it was accepted by the successor search).
    BadHeader,
    /// [`Scan::Failed`].
    Failed,
}

/// Size of the buffer `ExtAudioFile` reads packet groups into (`+0x58`); also what `AudioFileReadPacketData` may read.
const READ_BUFFER_BYTES: u32 = 65536;

/// State of the lazily built `AudioFile` packet table (`MPEGAudioFile` +0x78 table, +0x8c complete flag) seen by the
/// `AudioFileReadPacketData` calls of a reader that never queried the packet count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableState {
    /// Entries in the table (packets whose successor search succeeded or ended the stream).
    entries: Vec<Packet>,
    /// `MPEGAudioFile` +0x8c: the scan ended and was marked complete (only a scan called with the flag marks it).
    complete: bool,
}

impl TableState {
    /// Packets the table currently holds.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Table entry `i`.
    pub fn packet(&self, i: usize) -> Option<Packet> {
        self.entries.get(i).copied()
    }
}

/// The container view of an MP3 file, as reported through `ExtAudioFile` / `AudioFile`.
#[derive(Clone, Debug)]
pub struct Container {
    fmt: Fmt,
    file_size: u64,
    id3v1: bool,
    /// `kAudioFilePropertyDataOffset`: first byte of the first audio packet (after ID3v2 tag and Xing/VBRI frame).
    pub data_offset: u64,
    /// Size in bytes of the first accepted frame (the Xing/Info/VBRI frame when present).
    pub first_frame_size: u32,
    /// Xing/Info header of the first frame.
    pub xing: Option<Xing>,
    /// VBRI header found in the first frame: (frames, bytes).
    pub vbri: Option<(u32, u32)>,
    /// `+0xd0`: maximum frame size + 8 (`kAudioFilePropertyPacketSizeUpperBound`).
    pub packet_size_upper_bound: u32,
    num_packets_field: u64,
    audio_bytes: u32,
    stored_bitrate: u32,
    duration: f64,
    /// `+0x190/+0x198/+0x19c`; `valid_frames == -1` means "unset" (pnfo property fails with 'chk?').
    pnfo: PacketTableInfo,
    scan: Scan,
    scan_end: ScanEnd,
    /// OFL record found in the frame after the tag / in the first frame, applied by `PacketParsingComplete`.
    ofl_late: Option<(u16, u32)>,
}

impl Container {
    /// OSStatus of `ExtAudioFileSetProperty(ClientDataFormat)` for a same-rate float client: `'fmt?'` for MPEG-2.5 Layer I
    /// and II streams (AudioFile opens them, no converter exists), 0 otherwise.
    pub fn client_format_status(&self) -> i32 {
        if self.fmt.version == 3 && self.fmt.layer != 3 { STATUS_FMT } else { 0 }
    }

    /// The decoder-facing source format.
    pub fn source_format(&self) -> SourceFormat {
        SourceFormat {
            sample_rate: self.fmt.sample_rate,
            format_id: self.fmt.format_id,
            frames_per_packet: self.fmt.fpp,
            channels: self.fmt.channels,
            version: self.fmt.version,
            layer: self.fmt.layer,
        }
    }

    /// All AudioFile packets (complete scan); empty when the scan failed (see [`Self::readable_packets`]). The
    /// Xing/Info/VBRI frame is not a packet.
    pub fn packets(&self) -> &[Packet] {
        match &self.scan {
            Scan::Complete(p) => p,
            Scan::Failed { .. } => &[],
        }
    }

    /// The packets `ExtAudioFile` can actually read: the whole table of a complete scan, the `at` packets found before
    /// the failure otherwise (a group read that needs more fails with 'dta?', see [`Container::read_packets`]).
    pub fn readable_packets(&self) -> &[Packet] {
        match &self.scan {
            Scan::Complete(p) => p,
            Scan::Failed { readable, .. } => readable,
        }
    }

    /// How the packet scan ends (see [`ScanEnd`]).
    pub fn scan_end(&self) -> ScanEnd {
        self.scan_end
    }

    /// The scan outcome (see [`Scan`]).
    pub fn scan(&self) -> &Scan {
        &self.scan
    }

    /// `kAudioFilePropertyAudioDataByteCount`: size - data offset (- 128 with an ID3v1 tag).
    pub fn audio_data_byte_count(&self) -> i64 {
        let v = self.file_size as i64 - self.data_offset as i64;
        if self.id3v1 { v - 128 } else { v }
    }

    /// `kAudioFilePropertyPacketTableInfo`; `None` when Apple answers 'chk?'.
    pub fn packet_table_info(&self) -> Option<PacketTableInfo> {
        if self.pnfo.valid_frames == -1 { None } else { Some(self.pnfo) }
    }

    /// `kAudioFilePropertyPacketTableInfo` as an `AudioFile` that already completed a full scan answers it
    /// (`PacketParsingComplete`: an OFL record found in the first / second frame fills an unset table). The engine's
    /// `Reader` never sees this value: `ExtAudioFile` reads the table once, while opening.
    pub fn packet_table_info_after_scan(&self) -> Option<PacketTableInfo> {
        if let Some(p) = self.packet_table_info() {
            return Some(p);
        }
        let (prime, valid) = self.ofl_late?;
        let count = match &self.scan {
            Scan::Complete(p) => p.len() as i32,
            Scan::Failed { .. } => return None,
        };
        let rem = count.wrapping_mul(self.fmt.fpp as i32).wrapping_sub(valid.wrapping_add(u32::from(prime)) as i32);
        Some(PacketTableInfo { valid_frames: i64::from(valid), priming: i32::from(prime), remainder: rem.max(0) })
    }

    /// The table `AudioFile` reports right after `file_length_frames` ran (what `apple_probe`-style inspection shows).
    pub fn packet_table_info_after_length(&self) -> Option<PacketTableInfo> {
        if self.length_completes_scan() { self.packet_table_info_after_scan() } else { self.packet_table_info() }
    }

    /// `kAudioFilePropertyAudioDataPacketCount` of a freshly opened AudioFile (before any scan): the Xing / VBRI /
    /// CBR-estimate count when known, else the scanned count.
    pub fn packet_count_unscanned(&self) -> u64 {
        if self.num_packets_field != 0 { self.num_packets_field } else { self.packets().len() as u64 }
    }

    /// `ExtAudioFile` `kExtAudioFileProperty_FileLengthFrames` for a client format at the file's sample rate
    /// (`ExtAudioFile::GetNumberFrames`).
    pub fn file_length_frames(&self) -> i64 {
        match self.packet_table_info() {
            // (long)(ratio * (double)valid + 0.5) with ratio 1.0: truncation toward zero (negative valid -> +1)
            Some(p) => (p.valid_frames as f64 * 1.0 + 0.5) as i64,
            None => self.packet_count_unscanned() as i64 * i64::from(self.fmt.fpp),
        }
    }

    /// True when `file_length_frames` makes `MPEGAudioFile` scan the whole file (which changes `bitrate`).
    fn length_completes_scan(&self) -> bool {
        self.packet_table_info().is_none() && self.num_packets_field == 0
    }

    /// `AudioFileGetProperty(kAudioFilePropertyBitRate)` as queried by `Reader::bitrate` right after
    /// `file_length_frames` (the engine's inspect order): the Xing / VBRI / CBR-estimate derived value while the file
    /// has not been scanned completely, otherwise the integer mean of the header kbps of the scanned packets times
    /// 1000 (all packets when the length query scanned the file, else the first ten).
    pub fn bitrate(&self) -> u32 {
        let complete = self.length_completes_scan();
        if self.stored_bitrate != 0 && !complete {
            return self.stored_bitrate;
        }
        let packets = self.packets();
        let n = if complete { packets.len() } else { packets.len().min(10) };
        if n == 0 {
            return 0;
        }
        let sum: u64 = packets[..n].iter().map(|p| u64::from(p.kbps)).sum();
        ((sum / n as u64) as u32).wrapping_mul(1000)
    }

    /// Estimated duration stored from the Xing/VBRI header, seconds.
    pub fn stored_duration(&self) -> f64 {
        self.duration
    }

    /// `AudioFileReadPacketData(start, n)` exactly as `ExtAudioFile::ReadInputProc` issues it (packet descriptions and a
    /// buffer supplied), on a reader whose table is built lazily: `AudioFileObject::ReadPacketDataVBR` (0x19eb25a8c)
    ///
    /// 1. scans the table up to `start + 1` entries (`ScanForPackets(start + 1, file, 1)`; it may mark the table complete);
    /// 2. returns 0 packets when `start` is beyond the table (end of file, -39 reported as 0);
    /// 3. reads straight from the table when it already holds `start + n` entries;
    /// 4. otherwise reads up to 64 KiB of bytes from the first packet of the group into memory (`ReadBytes` clips the read
    ///    at the end of the AUDIO data: an ID3v1 tag is not part of it) and scans a second time over that buffer up to
    ///    `start + 1 + n` entries (`ScanForPackets(start + 1 + n, memory, 0)`, which never marks the table complete): a
    ///    group of more than one packet therefore needs the successor of its last packet to be scannable too. The scan
    ///    applies the ID3v1 rule (a candidate frame must end 128 bytes before the end of the data) to the end of the
    ///    memory data, which for a file with an ID3v1 tag is already 128 bytes short of the file end: such a scan sees the
    ///    end of the stream 256 bytes early.
    ///
    /// `data` is the file the container was parsed from. `Err('dta?')` when a scan hits the failure (the whole group is lost
    /// and the table is deleted); otherwise the number of packets delivered, `start..start + count` of `st` (fewer than `n`
    /// at the end of the stream).
    pub fn read_packets(&self, data: &[u8], st: &mut TableState, start: usize, n: usize) -> Result<usize, i32> {
        let file_size = data.len() as i64;
        self.scan_to(data, st, start + 1, file_size, true)?;
        if start >= st.entries.len() {
            return Ok(0);
        }
        if start + n <= st.entries.len() {
            return Ok(n);
        }
        // absolute end of the bytes read: 64 KiB from the first packet, at most the end of the audio data
        let audio_end = if self.id3v1 { file_size - 128 } else { file_size };
        let memory_end = (st.entries[start].offset as i64 + i64::from(READ_BUFFER_BYTES)).min(audio_end);
        self.scan_to(data, st, start + 1 + n, memory_end, false)?;
        Ok(n.min(st.entries.len() - start))
    }

    /// `MPEGAudioFile::ScanForPackets(goal, ds, flag)` (0x19ea996b8) on the lazily built table `st`: appends packets until the
    /// table holds `goal` entries or the scan ends. `size` = the size the data source reports (the end of the data),
    /// `flag` = the `param_4` that marks the table complete when the scan ends.
    fn scan_to(&self, data: &[u8], st: &mut TableState, goal: usize, size: i64, flag: bool) -> Result<(), i32> {
        if st.complete || st.entries.len() >= goal {
            return Ok(());
        }
        let data_end = if self.id3v1 { size - 128 } else { size };
        // the position of the packet to append next: the data offset, or the successor of the last entry (searched again)
        let mut pos = match st.entries.last() {
            None => self.data_offset,
            Some(last) => match next_matching_sync(data, self.id3v1, &self.fmt, (last.offset + u64::from(last.size)) as i64, size) {
                Ok(p) => p,
                Err(SyncErr::NoSync) => return Ok(()),
                Err(SyncErr::Dat) => {
                    *st = TableState::default(); // DeletePacketTable
                    return Err(STATUS_DAT);
                }
            },
        };
        while st.entries.len() < goal {
            let Some(d) = parse_at(data, pos) else { return Err(STATUS_DAT) };
            let end = pos as i64 + i64::from(d.size);
            if data_end < end {
                // the packet runs past the end of the data: not appended, the table is complete when flagged
                st.complete |= flag;
                return Ok(());
            }
            let pkt = Packet { offset: pos, size: d.size, kbps: d.kbps };
            match next_matching_sync(data, self.id3v1, &self.fmt, end, size) {
                Ok(p) => {
                    st.entries.push(pkt);
                    pos = p;
                }
                Err(SyncErr::NoSync) => {
                    st.entries.push(pkt);
                    st.complete = flag;
                    return Ok(());
                }
                Err(SyncErr::Dat) => {
                    if st.entries.len() >= 10 {
                        // treated like the end of the stream
                        st.entries.push(pkt);
                        st.complete = flag;
                        return Ok(());
                    }
                    *st = TableState::default(); // DeletePacketTable
                    return Err(STATUS_DAT);
                }
            }
        }
        Ok(())
    }
}

/// `MPEGAudioFile::ScanForPackets(-1)` from an empty table, plus how the scan terminated.
fn scan_packets(data: &[u8], id3v1: bool, fmt: &Fmt, data_offset: u64) -> (Scan, ScanEnd) {
    let size = data.len() as i64;
    let data_end = if id3v1 { size - 128 } else { size };
    let mut table: Vec<Packet> = Vec::new();
    let mut pos = data_offset;
    loop {
        let d = match parse_at(data, pos) {
            Some(d) => d,
            // 'dta?' returned directly (the table keeps what it has)
            None => return (Scan::Complete(table), ScanEnd::BadHeader),
        };
        let next = pos as i64 + i64::from(d.size);
        if data_end < next {
            return (Scan::Complete(table), ScanEnd::CrossEnd);
        }
        let pkt = Packet { offset: pos, size: d.size, kbps: d.kbps };
        match next_matching_sync(data, id3v1, fmt, next, size) {
            Ok(p) => {
                table.push(pkt);
                pos = p;
            }
            Err(SyncErr::NoSync) => {
                table.push(pkt);
                return (Scan::Complete(table), ScanEnd::NoSync);
            }
            Err(SyncErr::Dat) => {
                if table.len() >= 10 {
                    table.push(pkt);
                    return (Scan::Complete(table), ScanEnd::Fail10);
                }
                return (Scan::Failed { at: table.len(), readable: table }, ScanEnd::Failed);
            }
        }
    }
}

fn sat_u32(v: f64) -> u32 {
    if v >= 4294967295.0 { u32::MAX } else if v > 0.0 { v as u32 } else { 0 }
}

/// `gaSamplesPerFrame[(version-1)*3 + layer-1]`.
fn spf_table(version: u8, layer: u8) -> u32 {
    match (version, layer) {
        (_, 1) => 384,
        (1, _) => 1152,
        (_, 2) => 1152,
        _ => 576,
    }
}

/// Minimal C `sscanf("%X")` / `("%qX")` on a NUL terminated chunk: skips white space, optional sign and `0x`, parses hex
/// digits; no digits leaves the initial 0.
fn sscanf_hex(chunk: &[u8]) -> u64 {
    let mut i = 0;
    while i < chunk.len() && matches!(chunk[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < chunk.len() && (chunk[i] == b'+' || chunk[i] == b'-') {
        neg = chunk[i] == b'-';
        i += 1;
    }
    if i + 2 < chunk.len() && chunk[i] == b'0' && (chunk[i + 1] == b'x' || chunk[i + 1] == b'X') && chunk[i + 2].is_ascii_hexdigit() {
        i += 2;
    }
    let mut v: u64 = 0;
    while i < chunk.len() && chunk[i].is_ascii_hexdigit() {
        v = v.wrapping_mul(16).wrapping_add(u64::from((chunk[i] as char).to_digit(16).unwrap_or(0)));
        i += 1;
    }
    if neg { v.wrapping_neg() } else { v }
}

/// `ParseiTunesSMPBData(buf + 9, ...)`: `buf` is the NUL-less Latin-1 text of the comment.
fn parse_itunes_smpb(buf: &[u8]) -> Option<PacketTableInfo> {
    // the three tokens are read from fixed windows of the 128 byte buffer (zero padded)
    let mut b = [0u8; 128];
    b[..buf.len().min(127)].copy_from_slice(&buf[..buf.len().min(127)]);
    let win = |a: usize, n: usize| -> &[u8] {
        let w = &b[a..a + n];
        let end = w.iter().position(|&x| x == 0).unwrap_or(n);
        &w[..end]
    };
    let v1 = sscanf_hex(win(9, 9)) as u32;
    if v1 & 0x8000_0000 != 0 {
        return None;
    }
    let v2 = sscanf_hex(win(18, 9)) as u32;
    if v2 & 0x8000_0000 != 0 {
        return None;
    }
    let v3 = sscanf_hex(win(27, 17));
    if v3 & 0x8000_0000_0000_0000 != 0 {
        return None;
    }
    Some(PacketTableInfo { valid_frames: v3 as i64, priming: v1 as i32, remainder: v2 as i32 })
}

fn decode_id3_text(enc: u8, b: &[u8]) -> Vec<u16> {
    match enc {
        0 => b.iter().map(|&x| u16::from(x)).collect(),
        3 => String::from_utf8_lossy(b).encode_utf16().collect(),
        1 | 2 => {
            let (mut le, mut s) = (false, b);
            if enc == 1 {
                if s.len() >= 2 && s[0] == 0xff && s[1] == 0xfe {
                    le = true;
                    s = &s[2..];
                } else if s.len() >= 2 && s[0] == 0xfe && s[1] == 0xff {
                    s = &s[2..];
                }
            }
            s.chunks(2).filter(|c| c.len() == 2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect()
        }
        _ => Vec::new(),
    }
}

/// Known ID3v2 frame ids (`sID3FrameIDPairs`): a zero-size frame is only walked over when its id is one of these.
const ID3_V24_IDS: [&[u8; 4]; 92] = [
    b"AENC", b"APIC", b"ASPI", b"COMM", b"COMR", b"ENCR", b"EQU2", b"ETCO", b"GEOB", b"GRID", b"LINK", b"MCDI",
    b"MLLT", b"OWNE", b"PCST", b"PCNT", b"POPM", b"POSS", b"PRIV", b"RBUF", b"RVA2", b"RVRB", b"SEEK", b"SIGN",
    b"SYLT", b"SYTC", b"TALB", b"TBPM", b"TCAT", b"TCMP", b"TCOM", b"TCON", b"TCOP", b"TDEN", b"TDES", b"TDLY",
    b"TDOR", b"TDRC", b"TDRL", b"TDTG", b"TENC", b"TEXT", b"TFED", b"TFLT", b"TGID", b"TIPL", b"TIT1", b"TIT2",
    b"TIT3", b"TKEY", b"TKWD", b"TLAN", b"TLEN", b"TMCL", b"TMED", b"TMOO", b"TOAL", b"TOFN", b"TOLY", b"TOPE",
    b"TOWN", b"TPE1", b"TPE2", b"TPE3", b"TPE4", b"TPOS", b"TPRO", b"TPUB", b"TRCK", b"TRSN", b"TRSO", b"TSO2",
    b"TSOA", b"TSOC", b"TSOP", b"TSOT", b"TSRC", b"TSSE", b"TSST", b"TXXX", b"UFID", b"USER", b"USLT", b"WCOM",
    b"WCOP", b"WOAF", b"WOAR", b"WOAS", b"WORS", b"WPAY", b"WPUB", b"WXXX",
];
const ID3_V23_IDS: [&[u8; 4]; 83] = [
    b"AENC", b"APIC", b"COMM", b"COMR", b"ENCR", b"EQUA", b"ETCO", b"GEOB", b"GRID", b"LINK", b"MCDI", b"MLLT",
    b"OWNE", b"PCST", b"PCNT", b"POPM", b"POSS", b"PRIV", b"RBUF", b"RVAD", b"RVRB", b"SYLT", b"SYTC", b"TALB",
    b"TBPM", b"TCAT", b"TCMP", b"TCOM", b"TCON", b"TCOP", b"TDAT", b"TDES", b"TDLY", b"TYER", b"TENC", b"TEXT",
    b"TFED", b"TFLT", b"TGID", b"TIME", b"IPLS", b"TIT1", b"TIT2", b"TIT3", b"TKEY", b"TKWD", b"TLAN", b"TLEN",
    b"TMED", b"TOAL", b"TOFN", b"TOLY", b"TOPE", b"TORY", b"TOWN", b"TPE1", b"TPE2", b"TPE3", b"TPE4", b"TPOS",
    b"TPUB", b"TRCK", b"TRDA", b"TRSN", b"TRSO", b"TSIZ", b"TSO2", b"TSOC", b"TSRC", b"TSSE", b"TXXX", b"UFID",
    b"USER", b"USLT", b"WCOM", b"WCOP", b"WOAF", b"WOAR", b"WOAS", b"WORS", b"WPAY", b"WPUB", b"WXXX",
];
const ID3_V22_IDS: [&[u8; 3]; 72] = [
    b"CRA", b"PIC", b"COM", b"CRM", b"EQU", b"ETC", b"GEO", b"LNK", b"MCI", b"MLL", b"PCS", b"CNT", b"POP", b"BUF",
    b"RVA", b"REV", b"SLT", b"STC", b"TAL", b"TBP", b"TCT", b"TCP", b"TCM", b"TCO", b"TCR", b"TDA", b"TDS", b"TDY",
    b"TYE", b"TEN", b"TXT", b"TFD", b"TFT", b"TID", b"TIM", b"IPL", b"TT1", b"TT2", b"TT3", b"TKE", b"TKW", b"TLA",
    b"TLE", b"TMT", b"TOT", b"TOF", b"TOL", b"TOA", b"TOR", b"TP1", b"TP2", b"TP3", b"TP4", b"TPA", b"TPB", b"TRK",
    b"TRD", b"TSI", b"TS2", b"TSC", b"TRC", b"TSS", b"TXX", b"UFI", b"ULT", b"WCM", b"WCP", b"WAF", b"WAR", b"WAS",
    b"WPB", b"WXX",
];

fn id3_id_known(ver: u8, id: &[u8]) -> bool {
    match ver {
        4 => ID3_V24_IDS.iter().any(|k| k.as_slice() == id),
        3 => ID3_V23_IDS.iter().any(|k| k.as_slice() == id),
        _ => ID3_V22_IDS.iter().any(|k| k.as_slice() == id),
    }
}

/// ID3 unsynchronisation removal: `ff 00` -> `ff`.
fn id3_unsync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        out.push(b[i]);
        if b[i] == 0xff && i + 1 < b.len() && b[i + 1] == 0 {
            i += 1;
        }
        i += 1;
    }
    out
}

/// `GetPacketTableInfoFromID3`: first COMM frame of the leading ID3v2 tag whose description is "iTunSMPB" and whose text
/// (Latin-1, truncated at the first NUL, at most 127 chars, not the all-zero template) yields three non-negative hex
/// fields.
///
/// Frame walking as observed: ten byte (v2.2: six byte) frame headers read blindly from the end of the tag header (the
/// extended header flag is ignored); size 0 ends the walk unless the id is a known frame id; a frame that does not fit in the
/// tag ends the walk; COMM frames flagged compressed/encrypted (v2.3: 0x80/0x40, v2.4: 0x08/0x04) or with a data length
/// indicator (v2.4: 0x01) are skipped; an all-zero iTunSMPB comment ends the search. Unsynchronisation (`ff 00` -> `ff`)
/// is removed from the whole tag body for v2.2/v2.3 (header flag) and per frame body for v2.4 (header or frame flag).
fn id3_itunes_smpb(data: &[u8]) -> Option<PacketTableInfo> {
    let tsiz = id3v2_size(data)? as usize;
    if data.len() < 10 || &data[..3] != b"ID3" {
        return None;
    }
    let ver = data[3];
    let tag_unsync = data[5] & 0x80 != 0;
    // v2.2 / v2.3: the whole tag body is unsynchronised, so frames are walked on the decoded bytes
    let decoded: Vec<u8>;
    let data: &[u8] = if tag_unsync && ver <= 3 {
        let e = tsiz.min(data.len());
        let mut v = data[..10].to_vec();
        v.extend_from_slice(&id3_unsync(&data[10..e]));
        decoded = v;
        &decoded
    } else {
        data
    };
    let (idlen, hdr) = if ver <= 2 { (3, 6) } else { (4, 10) };
    let end = if tag_unsync && ver <= 3 { data.len() } else { tsiz.min(data.len()) };
    let mut pos = 10usize;
    let zero_template: &[u8] = b" 00000000 00000000 00000000 0000000000000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000";
    while pos + hdr <= end {
        let id = &data[pos..pos + idlen];
        let size = if ver <= 2 {
            ((data[pos + 3] as usize) << 16) | ((data[pos + 4] as usize) << 8) | data[pos + 5] as usize
        } else {
            let raw = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
            if ver == 4 && raw > 0x7f && raw & 0x8080_8080 == 0 {
                (((raw >> 24) & 0x7f) << 21 | ((raw >> 16) & 0x7f) << 14 | ((raw >> 8) & 0x7f) << 7 | (raw & 0x7f)) as usize
            } else {
                raw as usize
            }
        };
        if size == 0 && !id3_id_known(ver, id) {
            break;
        }
        let body_start = pos + hdr;
        let body_end = match body_start.checked_add(size) {
            Some(e) if e <= end => e,
            _ => break,
        };
        let is_comm = if ver <= 2 { id == b"COM" } else { id == b"COMM" };
        let fl2 = if ver >= 3 { data[pos + 9] } else { 0 };
        let skipped = match ver {
            3 => fl2 & 0xc0 != 0,
            4 => fl2 & 0x0d != 0,
            _ => false,
        };
        if is_comm && !skipped && body_end >= body_start + 4 {
            let raw_body = &data[body_start..body_end];
            // v2.4: per-frame unsynchronisation (tag flag or frame flag 0x02)
            let unsynced;
            let body: &[u8] = if ver == 4 && (tag_unsync || fl2 & 0x02 != 0) {
                unsynced = id3_unsync(raw_body);
                &unsynced
            } else {
                raw_body
            };
            if body.len() < 4 {
                pos = body_end;
                continue;
            }
            let enc = body[0];
            let rest = &body[4..];
            // description: up to the (double) NUL terminator
            let wide = enc == 1 || enc == 2;
            let mut d_end = None;
            if wide {
                let mut i = 0;
                while i + 2 <= rest.len() {
                    if rest[i] == 0 && rest[i + 1] == 0 {
                        d_end = Some(i);
                        break;
                    }
                    i += 2;
                }
            } else {
                d_end = rest.iter().position(|&x| x == 0);
            }
            if let Some(de) = d_end {
                let desc = decode_id3_text(enc, &rest[..de]);
                let text_bytes = &rest[(de + if wide { 2 } else { 1 })..];
                if desc.iter().copied().eq("iTunSMPB".encode_utf16()) {
                    let mut text = decode_id3_text(enc, text_bytes);
                    if let Some(z) = text.iter().position(|&c| c == 0) {
                        text.truncate(z);
                    }
                    if text.iter().all(|&c| c <= 0xff) && text.len() <= 127 {
                        let latin: Vec<u8> = text.iter().map(|&c| c as u8).collect();
                        if latin.as_slice() == zero_template {
                            return None;
                        }
                        if let Some(p) = parse_itunes_smpb(&latin) {
                            return Some(p);
                        }
                    }
                }
            }
        }
        pos = body_end;
    }
    None
}

/// `ID3ParserOpenWithDataSource` + `tsiz`: size of the leading ID3v2 tag (footer included when flagged) or 128 for a
/// leading 'TAG'. `None` when the parser refuses it (tiny tags, no tag).
pub fn id3v2_size(data: &[u8]) -> Option<i64> {
    if data.len() < 10 {
        return None;
    }
    if &data[..3] == b"TAG" {
        return Some(128);
    }
    if &data[..3] == b"ID3" {
        let flags = data[5];
        let sz = ((u32::from(data[6]) & 0x7f) << 21) | ((u32::from(data[7]) & 0x7f) << 14) | ((u32::from(data[8]) & 0x7f) << 7) | (u32::from(data[9]) & 0x7f);
        let total = if flags & 0x10 != 0 { sz + 20 } else { sz + 10 };
        let hdr = if data[3] > 2 { 10 } else { 6 };
        if hdr + 11 <= total {
            return Some(i64::from(total));
        }
    }
    None
}

/// CRC-8 (polynomial 0x45, init 0xff) used by the OFL record.
fn ofl_crc8(b: &[u8]) -> u8 {
    let mut crc = 0xffu8;
    for &x in b {
        crc ^= x;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x45 } else { crc << 1 };
        }
    }
    crc
}

fn bits_at(b: &[u8], pos: usize, n: usize) -> Option<u32> {
    let mut v = 0u32;
    for i in 0..n {
        let p = pos + i;
        let byte = *b.get(p / 8)?;
        v = (v << 1) | u32::from((byte >> (7 - p % 8)) & 1);
    }
    Some(v)
}

/// `MPEGAudioOFLInfo::Deserialize`: gapless record `(priming, valid frames)` of an Apple-encoded MP3. Only MPEG-1 frames
/// qualify. Apple tests the header's *private* bit (not the protection bit) to decide whether to skip 16 bits (a CRC) before
/// the side info, so a CRC-protected frame with private = 0 is read from the wrong place. The record sits right after the
/// frame's own main data (the sum of the side info `part2_3_length` fields, `main_data_begin` ignored), starting at the next
/// byte boundary: `0xb4`, priming + 528 (u16), valid frames (u32), two more bytes, CRC-8 over the first nine bytes. At least
/// ten bytes must remain after the main data.
pub fn ofl_info(frame: &[u8]) -> Option<(u16, u32)> {
    if frame.len() < 4 {
        return None;
    }
    let w = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
    let d = parse_sync_word(w)?;
    if d.version != 1 {
        return None;
    }
    let side_fixed = 9 + if d.channels == 1 { 5 } else { 3 } + 4 * d.channels as usize;
    let mut p = 32 + side_fixed + if w & 0x100 != 0 { 16 } else { 0 };
    let mut sum = 0usize;
    for _ in 0..2 * d.channels as usize {
        sum += bits_at(frame, p, 12)? as usize;
        p += 59;
    }
    if frame.len() * 8 < p + sum + 80 {
        return None;
    }
    let off = (p + sum).div_ceil(8);
    let rec = frame.get(off..off + 10)?;
    if rec[0] != 0xb4 || ofl_crc8(&rec[..9]) != rec[9] {
        return None;
    }
    let v16 = u16::from_be_bytes([rec[1], rec[2]]);
    let valid = u32::from_be_bytes([rec[3], rec[4], rec[5], rec[6]]);
    let prime = if v16 >= 0x210 { v16 - 0x210 } else { v16 };
    Some((prime, valid))
}

/// Parse an MP3 file image like `AudioFileOpenURL` does. Returns `Err` when Apple refuses the file.
pub fn parse(data: &[u8]) -> Result<Container, Refusal> {
    generic_open_check(data)?;
    let size = data.len() as i64;
    let id3v1 = id3v1_exists(data);

    // ---- ParseID3Tags + start offset (ID3v2 at the start of the file, plus chained "ID3" headers) ----
    let mut start: i64 = 0;
    if let Some(tsiz) = id3v2_size(data) {
        let mut pos = tsiz;
        let mut extras: i64 = 0;
        loop {
            let p = pos as usize;
            if data.len() < p + 10 || &data[p..p + 3] != b"ID3" {
                break;
            }
            let h = &data[p..p + 10];
            if h[3] == 0xff || h[4] == 0xff {
                return Err(Refusal::Hang);
            }
            let sz = i64::from(
                ((u32::from(h[6]) & 0x7f) << 21) | ((u32::from(h[7]) & 0x7f) << 14) | ((u32::from(h[8]) & 0x7f) << 7) | (u32::from(h[9]) & 0x7f),
            ) + 10;
            extras += sz;
            pos += sz;
        }
        start = tsiz + extras;
    }

    // ---- state machine of OpenFromDataSource: find a stream start confirmed by up to four following frames ----
    let mut tries = 0u32;
    let mut scan_start = start;
    let (first_pos, first) = loop {
        let cand = match next_sync(data, id3v1, scan_start, size) {
            Ok(f) => f,
            Err(_) => return Err(Refusal::Dat),
        };
        scan_start = cand.pos as i64;
        let d1 = cand.desc;
        let mut gaps = 0u32;
        let mut matches = 0u32;
        let mut p = cand.pos as i64 + i64::from(d1.size);
        let ok = loop {
            match next_sync(data, id3v1, p, size) {
                Err(_) => break true, // end of data: accept
                Ok(f) => {
                    if f.pos as i64 != p {
                        let before = gaps;
                        gaps += 1;
                        p = f.pos as i64;
                        if before >= 3 {
                            break false;
                        }
                    }
                    let d = f.desc;
                    if d.version == d1.version && d.layer == d1.layer && d.sample_rate == d1.sample_rate && d.channels == d1.channels {
                        matches += 1;
                        p += i64::from(d.size);
                        if matches == 4 {
                            break true;
                        }
                    } else {
                        gaps += 1;
                        if gaps > 3 {
                            break false;
                        }
                        p += 1;
                    }
                }
            }
        };
        if ok {
            break (cand.pos, d1);
        }
        tries += 1;
        if tries > 9 {
            return Err(Refusal::Dat);
        }
        scan_start += 1;
    };

    // ---- ParseFirstPacket ----
    let data_end = if id3v1 { size - 128 } else { size };
    let first_size = i64::from(first.size);
    if first.size == 0 || first_pos as i64 + first_size > data_end {
        return Err(Refusal::Dat);
    }
    let fmt = Fmt {
        version: first.version,
        layer: first.layer,
        sample_rate: first.sample_rate,
        channels: first.channels,
        fpp: first.frames_per_packet,
        format_id: first.format_id,
    };
    let first_bytes = &data[first_pos as usize..first_pos as usize + first.size as usize];

    let mut c = Container {
        fmt,
        file_size: data.len() as u64,
        id3v1,
        data_offset: first_pos,
        first_frame_size: first.size,
        xing: None,
        vbri: None,
        packet_size_upper_bound: first.max_size + 8,
        num_packets_field: 0,
        audio_bytes: 0,
        stored_bitrate: 0,
        duration: 0.0,
        pnfo: id3_itunes_smpb(data).unwrap_or(PacketTableInfo { valid_frames: -1, priming: 0, remainder: 0 }),
        scan: Scan::Complete(Vec::new()),
        scan_end: ScanEnd::NoSync,
        ofl_late: None,
    };
    let rate = f64::from(fmt.sample_rate);
    let fpp = u64::from(fmt.fpp);
    let after_first = first_pos as i64 + first_size;

    let mut follow: Option<bool> = None;
    if let Some(x) = get_meta_header(first_bytes) {
        c.xing = Some(x);
        let f = x.flags;
        if f & 2 != 0 {
            c.audio_bytes = x.bytes.saturating_sub(first.size);
        }
        let mut bitrate_calc = false;
        let mut set_valid = false;
        if f & 1 != 0 {
            c.num_packets_field = u64::from(x.frames);
            c.duration = f64::from(spf_table(fmt.version, fmt.layer)) / rate * f64::from(x.frames);
            if c.pnfo.valid_frames == -1 && f & 0x20 != 0 {
                c.pnfo.priming = i32::from(x.delay);
                c.pnfo.remainder = i32::from(x.padding);
                set_valid = true;
            }
            bitrate_calc = true;
        } else if f & 0x10 == 0 {
            if c.pnfo.valid_frames == -1 && f & 0x20 != 0 {
                c.pnfo.priming = i32::from(x.delay);
                c.pnfo.remainder = i32::from(x.padding);
            }
        } else {
            // "Info" tag without frame count: estimate from the byte count and the next frame's bit rate
            let nm = next_matching_sync(data, id3v1, &fmt, after_first, size).map_err(SyncErr::refusal)?;
            let d2 = parse_at(data, nm).ok_or(Refusal::Dat)?;
            let no_bytes = f & 2 == 0;
            if !no_bytes {
                let d = f64::from(c.audio_bytes) / (f64::from(d2.bitrate_bps) / 8.0);
                c.duration = d;
                c.num_packets_field = (d * (rate / f64::from(fmt.fpp))).ceil() as u64;
            }
            if c.pnfo.valid_frames == -1 && f & 0x20 != 0 {
                c.pnfo.priming = i32::from(x.delay);
                c.pnfo.remainder = i32::from(x.padding);
                if !no_bytes {
                    set_valid = true;
                    bitrate_calc = true;
                }
            } else if !no_bytes {
                bitrate_calc = true;
            }
        }
        if set_valid {
            c.pnfo.valid_frames = c
                .num_packets_field
                .wrapping_mul(fpp)
                .wrapping_sub(u64::from(c.pnfo.remainder as u32))
                .wrapping_sub(u64::from(c.pnfo.priming as u32)) as i64;
        }
        if bitrate_calc && c.duration > 0.0 {
            c.stored_bitrate = sat_u32(f64::from(c.audio_bytes) * 8.0 / c.duration);
        }
        follow = Some(bitrate_calc);
    } else if let Some((frames, bytes, quality)) = parse_vbri(first_bytes) {
        c.vbri = Some((frames, bytes));
        c.num_packets_field = u64::from(frames).saturating_sub(1);
        c.audio_bytes = bytes;
        // ParseVBRIPacket stores the header's 16 bit field at +2 (quality) in the bit rate slot
        c.stored_bitrate = u32::from(quality);
        if c.audio_bytes > first.size {
            c.audio_bytes -= first.size;
        }
        c.duration = (c.num_packets_field * fpp) as f64 / rate;
        if c.duration > 0.0 {
            c.stored_bitrate = sat_u32(f64::from(c.audio_bytes) * 8.0 / c.duration);
        }
        follow = Some(true);
    }

    if let Some(uvar18) = follow {
        // the first real audio frame after the tag frame becomes the data offset. The search writes its out parameter on
        // every candidate it finds, so after a failed search the offset is the last (non-matching) candidate, or the
        // end of the tag frame when there was none. The status only matters when the packet table is already known.
        let (r, out) = next_matching_sync_out(data, id3v1, &fmt, after_first, size);
        c.data_offset = out as u64;
        let p = out as u64;
        if c.pnfo.valid_frames != -1 {
            r.map_err(SyncErr::refusal)?;
        } else {
            // the frame at the offset must be a valid packet
            let d = parse_at(data, p).ok_or(Refusal::Dat)?;
            if d.size == 0 || p as i64 + i64::from(d.size) > data_end {
                return Err(Refusal::Dat);
            }
            // Apple-encoder gapless record ("OFL") in that frame. After a Xing/VBRI frame whose bit rate / duration were
            // derived (uVar18) it becomes the packet table info at once, otherwise only when the scan completes.
            if let Some((prime, valid)) = ofl_info(&data[p as usize..p as usize + d.size as usize]) {
                if uvar18 {
                    c.pnfo.priming = i32::from(prime);
                    c.pnfo.valid_frames = i64::from(valid);
                    let rem = (c.num_packets_field as u32).wrapping_mul(fmt.fpp).wrapping_sub(valid.wrapping_add(u32::from(prime))) as i32;
                    c.pnfo.remainder = rem.max(0);
                } else {
                    c.ofl_late = Some((prime, valid));
                }
            }
        }
    } else if c.pnfo.valid_frames == -1 {
        // no tag frame: the OFL record of the first frame only counts once the scan completes
        c.ofl_late = ofl_info(first_bytes);
    }

    let (scan, scan_end) = scan_packets(data, id3v1, &fmt, c.data_offset);
    c.scan = scan;
    c.scan_end = scan_end;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(ver_bits: u32, layer_bits: u32, bitrate_idx: u32, rate_idx: u32, pad: u32, mode: u32) -> [u8; 4] {
        let w = (0x7ffu32 << 21) | (ver_bits << 19) | (layer_bits << 17) | (1 << 16) | (bitrate_idx << 12) | (rate_idx << 10) | (pad << 9) | (mode << 6);
        w.to_be_bytes()
    }

    /// MPEG-1 Layer III stereo 128 kbps 44.1 kHz frames (417 bytes, 418 with padding).
    fn l3_stream(n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut err = 0u32;
        for _ in 0..n {
            // CBR padding pattern: 144 * 128000 / 44100 = 417.96
            err += 96; // fractional part in 1/100 byte
            let pad = u32::from(err >= 100);
            if pad == 1 {
                err -= 100;
            }
            let h = header(3, 1, 9, 0, pad, 1);
            out.extend_from_slice(&h);
            out.resize(out.len() + 413 + pad as usize, 0);
        }
        out
    }

    #[test]
    fn frame_sizes() {
        let d = parse_sync_word(u32::from_be_bytes(header(3, 1, 9, 0, 0, 1))).unwrap();
        assert_eq!((d.size, d.frames_per_packet, d.channels, d.sample_rate, d.format_id), (417, 1152, 2, 44100, FORMAT_MP3));
        let d = parse_sync_word(u32::from_be_bytes(header(2, 1, 8, 0, 1, 3))).unwrap(); // MPEG-2 64 kbps 22.05 kHz mono, padded
        assert_eq!((d.size, d.frames_per_packet, d.channels), (209, 576, 1));
        let d = parse_sync_word(u32::from_be_bytes(header(3, 2, 8, 0, 0, 1))).unwrap(); // Layer II 128 kbps
        assert_eq!((d.size, d.format_id), (417, FORMAT_MP2));
        let d = parse_sync_word(u32::from_be_bytes(header(3, 3, 4, 0, 1, 1))).unwrap(); // Layer I 128 kbps padded
        assert_eq!((d.size, d.frames_per_packet), (140, 384));
        // refused: free format, bad bitrate index, reserved sample rate / version / layer
        for w in [header(3, 1, 0, 0, 0, 1), header(3, 1, 15, 0, 0, 1), header(3, 1, 9, 3, 0, 1), header(1, 1, 9, 0, 0, 1), header(3, 0, 9, 0, 0, 1)] {
            assert!(parse_sync_word(u32::from_be_bytes(w)).is_none());
        }
    }

    #[test]
    fn plain_stream_packets_length_bitrate() {
        let s = l3_stream(20);
        let c = parse(&s).unwrap();
        assert_eq!(c.data_offset, 0);
        assert_eq!(c.packets().len(), 20);
        assert_eq!(c.packets()[1].offset, 417);
        assert!(c.packet_table_info().is_none());
        // no tag: the length query scans everything; bit rate = mean header kbps of all packets
        assert_eq!(c.file_length_frames(), 20 * 1152);
        assert_eq!(c.bitrate(), 128_000);
        assert_eq!(c.source_format(), SourceFormat { sample_rate: 44100, format_id: FORMAT_MP3, frames_per_packet: 1152, channels: 2, version: 1, layer: 3 });
    }

    #[test]
    fn truncated_last_frame_is_dropped_and_leading_junk_skipped() {
        let mut s = vec![0x11u8; 100];
        s.extend(l3_stream(10));
        s.truncate(s.len() - 5);
        let c = parse(&s).unwrap();
        assert_eq!(c.data_offset, 100);
        assert_eq!(c.packets().len(), 9);
    }

    #[test]
    fn id3v2_is_skipped_and_id3v1_ends_the_data() {
        let mut tag = b"ID3\x03\x00\x00\x00\x00\x00\x20".to_vec();
        tag.resize(10 + 32, 0);
        let mut s = tag.clone();
        s.extend(l3_stream(8));
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, 0);
        s.extend(&v1);
        let c = parse(&s).unwrap();
        assert_eq!(c.data_offset, 42);
        assert_eq!(c.packets().len(), 8);
        assert_eq!(c.audio_data_byte_count(), s.len() as i64 - 42 - 128);
    }

    #[test]
    fn lame_info_tag_gives_priming_and_valid_frames() {
        // Info frame (stereo MPEG-1: tag at 36) with frames, bytes, LAME tag with delay 576 / padding 1000
        let mut f = l3_stream(1);
        let n_audio = 30u32;
        let mut t = f[..417].to_vec();
        for b in t.iter_mut().skip(4) {
            *b = 0;
        }
        t[36..40].copy_from_slice(b"Info");
        t[40..44].copy_from_slice(&3u32.to_be_bytes()); // frames + bytes
        t[44..48].copy_from_slice(&n_audio.to_be_bytes());
        t[48..52].copy_from_slice(&(417u32 * 31).to_be_bytes());
        let lame = 52;
        t[lame..lame + 9].copy_from_slice(b"LAME3.100");
        let dp = (576u32 << 12) | 1000;
        t[lame + 0x15..lame + 0x18].copy_from_slice(&dp.to_be_bytes()[1..]);
        f.clear();
        f.extend(&t);
        f.extend(l3_stream(30));
        let c = parse(&f).unwrap();
        assert_eq!(c.data_offset, 417);
        let p = c.packet_table_info().unwrap();
        assert_eq!((p.priming, p.remainder, p.valid_frames), (576, 1000, 30 * 1152 - 1576));
        assert_eq!(c.file_length_frames(), 30 * 1152 - 1576);
    }

    #[test]
    fn refusals_and_statuses() {
        assert_eq!(parse(&[]).unwrap_err().os_status(), STATUS_WHT);
        assert_eq!(parse(&[0x55; 100]).unwrap_err().os_status(), STATUS_DAT);
        // an Info frame with no audio after it
        let mut t = l3_stream(1);
        t[36..40].copy_from_slice(b"Info");
        assert_eq!(parse(&t).unwrap_err().os_status(), STATUS_NO_SYNC);
    }

    /// `k` Layer III frames, then the junk that makes the successor search of the last frame fail (40 headers of another
    /// sample rate), then 20 more frames: the packet scan ends after `k - 1` readable packets (`Scan::Failed`) when `k < 11`
    /// and by the ten-candidates rule (`ScanEnd::Fail10`) otherwise.
    fn broken_stream(k: usize) -> Vec<u8> {
        let mut s = l3_stream(k);
        for _ in 0..40 {
            s.extend_from_slice(&[0xff, 0xfb, 0x94, 0x04]);
        }
        s.extend(l3_stream(20));
        s
    }

    /// `AudioFileReadPacketData(start, n)` on a fresh table: `Some(packets)` or `None` for 'dta?'.
    fn fresh_read(d: &[u8], c: &Container, start: usize, n: usize) -> Option<usize> {
        c.read_packets(d, &mut TableState::default(), start, n).ok()
    }

    #[test]
    fn failed_scan_group_reads_need_the_successor_of_a_group_of_two_or_more_packets() {
        // measured with AudioFileReadPacketData on macOS 27.2 (fresh AudioFile each): packets 0..=3 are readable one by one,
        // a group of two or more needs one more table entry than it reads
        let d = broken_stream(5);
        let c = parse(&d).unwrap();
        assert!(matches!(c.scan(), Scan::Failed { at: 4, .. }));
        for start in 0..4 {
            assert_eq!(fresh_read(&d, &c, start, 1), Some(1), "(start {start}, 1)");
        }
        assert_eq!(fresh_read(&d, &c, 4, 1), None);
        for (n, ok_starts) in [(2usize, 0..2usize), (3, 0..1), (4, 0..0)] {
            for start in 0..6 {
                assert_eq!(fresh_read(&d, &c, start, n).is_some(), ok_starts.contains(&start), "(start {start}, {n})");
            }
        }
        let d = broken_stream(10);
        let c = parse(&d).unwrap();
        assert!(matches!(c.scan(), Scan::Failed { at: 9, .. }));
        for (n, ok_starts) in [(1usize, 0..9usize), (2, 0..7), (3, 0..6), (7, 0..2), (8, 0..1)] {
            for start in 0..11 {
                assert_eq!(fresh_read(&d, &c, start, n).is_some(), ok_starts.contains(&start), "(start {start}, {n})");
            }
        }
    }

    #[test]
    fn lazy_table_remembers_how_far_it_scanned() {
        let d = broken_stream(5);
        let c = parse(&d).unwrap();
        let mut st = TableState::default();
        assert_eq!(c.read_packets(&d, &mut st, 0, 2), Ok(2));
        assert_eq!(st.count(), 3); // the second scan went one packet past the group
        // (2, 2) alone would fail too; after (0, 2) the table holds 3 entries and the group needs 5
        assert_eq!(c.read_packets(&d, &mut st, 2, 2), Err(STATUS_DAT));
        assert_eq!(st.count(), 0); // the failed scan deleted the table
        // single packet reads never scan past their own packet
        let mut st = TableState::default();
        for p in 0..4 {
            assert_eq!(c.read_packets(&d, &mut st, p, 1), Ok(1));
        }
        assert_eq!(c.read_packets(&d, &mut st, 4, 1), Err(STATUS_DAT));
    }

    #[test]
    fn ten_candidate_failure_ends_the_table_but_only_a_flagged_scan_completes_it() {
        let d = broken_stream(11);
        let c = parse(&d).unwrap();
        assert_eq!(c.scan_end(), ScanEnd::Fail10);
        assert_eq!(c.packets().len(), 11);
        // one packet at a time: the flagged scan reaches the end of the stream and marks the table complete
        let mut st = TableState::default();
        for p in 0..11 {
            assert_eq!(c.read_packets(&d, &mut st, p, 1), Ok(1));
        }
        assert_eq!(c.read_packets(&d, &mut st, 11, 1), Ok(0));
        // groups of two: the second scans (never flagged) run into the end; the next read starts over and fails, the retry succeeds
        let mut st = TableState::default();
        for start in [0, 2, 4, 6, 8] {
            assert_eq!(c.read_packets(&d, &mut st, start, 2), Ok(2), "start {start}");
        }
        assert_eq!(st.count(), 11);
        assert_eq!(c.read_packets(&d, &mut st, 10, 2), Err(STATUS_DAT));
        assert_eq!(c.read_packets(&d, &mut st, 10, 2), Ok(1));
        assert_eq!(c.read_packets(&d, &mut st, 12, 2), Ok(0));
        // a fresh table: the group at the end is shortened without error
        assert_eq!(fresh_read(&d, &c, 10, 2), Some(1));
        assert_eq!(fresh_read(&d, &c, 9, 2), Some(2));
        assert_eq!(fresh_read(&d, &c, 11, 2), Some(0));
    }

    #[test]
    fn clean_stream_group_reads_are_shortened_at_the_end() {
        let d = l3_stream(20);
        let c = parse(&d).unwrap();
        assert_eq!(c.scan_end(), ScanEnd::NoSync);
        let mut st = TableState::default();
        assert_eq!(c.read_packets(&d, &mut st, 0, 8), Ok(8));
        assert_eq!(c.read_packets(&d, &mut st, 18, 8), Ok(2));
        assert_eq!(c.read_packets(&d, &mut st, 20, 8), Ok(0));
    }

    #[test]
    fn lazily_scanned_table_equals_the_full_scan() {
        let d = l3_stream(40);
        let c = parse(&d).unwrap();
        let mut st = TableState::default();
        let mut start = 0;
        while let Ok(n) = c.read_packets(&d, &mut st, start, 3) {
            if n == 0 {
                break;
            }
            start += n;
        }
        let lazy: Vec<_> = (0..st.count()).map(|i| st.packet(i).unwrap()).collect();
        assert_eq!(lazy.as_slice(), c.packets());
    }

    #[test]
    fn id3v1_tag_shortens_the_memory_scan_by_256_bytes() {
        // 6 frames, 40 foreign headers (candidates of 384 bytes at 2506, 2510, ..), `t` zero bytes, an ID3v1 tag. Measured with
        // AudioFileReadPacketData (fresh AudioFile each): the file scan fails for packet 5 once the tenth candidate (at 2542,
        // ending at 2926) lies inside the data, i.e. from a file size of 2926 + 128; the memory scan of a group of several
        // packets fails once the data ends 256 bytes after that point instead of 128 (the tag is clipped twice).
        let build = |t: usize| {
            let mut s = l3_stream(6);
            for _ in 0..40 {
                s.extend_from_slice(&[0xff, 0xfb, 0x94, 0x04]);
            }
            s.resize(s.len() + t, 0);
            let mut tag = b"TAG".to_vec();
            tag.resize(128, 0);
            s.extend(tag);
            s
        };
        // (t, (5,1) readable, (0,5) / (1,4) / (2,3) readable)
        for (t, single, groups) in [(250usize, true, true), (300, false, true), (500, false, false), (3000, false, false)] {
            let d = build(t);
            let c = parse(&d).unwrap();
            assert_eq!(fresh_read(&d, &c, 5, 1).is_some(), single, "t {t} (5,1)");
            for (start, n) in [(0, 5), (1, 4), (2, 3)] {
                assert_eq!(fresh_read(&d, &c, start, n).is_some(), groups, "t {t} ({start},{n})");
            }
        }
    }
}
