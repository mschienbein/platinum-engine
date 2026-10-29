//! Portable MP3 reader backend (opt-in; not byte-identical to AudioToolbox).
//!
//! The container model reproduces what AudioToolbox's MP3 file reader reports
//! and delivers for the stream layouts that were compared with it on macOS
//! (see `local-artifacts/portable-parity/mp3dec-01` and `mp3dec-02`): packet
//! counts from Xing/Info/VBRI headers or a frame scan, LAME encoder delay and
//! padding, the 529-sample decoder delay, zero filling of the final 529
//! samples, a truncated final frame, and 16-bit saturated output. Layouts that
//! were not matched are refused with a typed error rather than guessed.
//!
//! Only a confirmed MP3 stream is treated as MP3: a frame header that starts a
//! run of consistent headers (see `confirm_stream`), after an ID3v2 tag and at
//! most `LEADING_JUNK_LIMIT` bytes of junk without frame-like bytes. Everything
//! else is `NotMp3`, so the caller's own classification and error stand.
//!
//! The Layer III decoder itself (`mp3_l3`) is a from-scratch implementation of
//! ISO/IEC 11172-3 and 13818-3 in safe Rust using only IEEE-754 binary64 basic
//! operations, so its output is bit-identical on every host.
//!
//! The samples are not bit-exact with AudioToolbox, whose decoder is fixed point
//! and delivers 16-bit integers: the portable output is rounded onto the same
//! 16-bit grid and, on the compared corpus, differs from AudioToolbox by at most
//! 4 LSB (95% of samples are identical; 1 LSB at most away on normal levels).
//! Mixed long/short blocks, MPEG-2 intensity stereo, Layer I/II, free-format
//! streams, corrupt frames and anything that would need sample-rate conversion
//! are refused, not approximated.

#[path = "mp3_tables.rs"]
mod mp3_tables;
#[path = "mp3_math.rs"]
mod mp3_math;
#[path = "mp3_l3.rs"]
mod mp3_l3;

use mp3_l3::{Damage, Decoder, Header, Pcm, Version, GRANULE};
use std::fs::File;
use std::io::{self, BufReader, Read};

/// Samples AudioToolbox's MP3 decoder discards at the start (its filterbank delay).
const DECODER_DELAY: u64 = 529;
/// Longest run of non-frame bytes accepted between the ID3v2 tag and the first frame.
const LEADING_JUNK_LIMIT: u64 = 1 << 20;

#[derive(Debug)]
pub enum Mp3Error {
    /// The data does not start like an MP3 stream at all.
    NotMp3,
    /// A valid MP3 variant whose Apple behavior is not reproduced.
    Unsupported(&'static str),
    /// Damaged or inconsistent data.
    Corrupt(&'static str),
    Io(&'static str, io::Error),
}

/// What the file looks like to a client of ExtAudioFile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mp3Info {
    pub sample_rate: u32,
    pub channels: u32,
    pub frames_per_packet: u32,
    /// The file-length-frames property (can differ from the frames delivered).
    pub length_frames: i64,
    /// The `brat` property.
    pub bitrate: u32,
}

#[derive(Clone, Copy, Debug)]
enum Tag {
    None,
    /// `frames` is the Xing/Info frame count, `bytes` the optional byte count.
    Xing { info: bool, lame: bool, frames: Option<u32>, bytes: Option<u32>, delay: u32, padding: u32, size: u32 },
    /// Fraunhofer VBRI header (byte count, frame count including this frame, and this frame's size).
    Vbri { bytes: u32, frames: u32, size: u32 },
}

pub struct Mp3Stream {
    src: Rd,
    info: Mp3Info,
    first: Header,
    /// File offset of the next undecoded frame.
    next_frame: u64,
    frames_total: u64,
    frames_decoded: u64,
    decoder: Decoder,
    pcm: Box<Pcm>,
    /// Decoded, quantized samples of the current frame.
    cur: [Vec<f32>; 2],
    cur_start: u64,
    skip: u64,
    out_len: u64,
    out_pos: u64,
    frame_buf: Vec<u8>,
    spf: u64,
    channels: usize,
    seen_good_frame: bool,
}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> Mp3Error { move |e| Mp3Error::Io(what, e) }

/// Buffered file with a tracked position; seeking within the buffer is free.
struct Rd { r: BufReader<File>, pos: u64 }
impl Rd {
    fn new(file: File) -> Rd { Rd { r: BufReader::with_capacity(64 * 1024, file), pos: 0 } }
    fn seek(&mut self, offset: u64, what: &'static str) -> Result<(), Mp3Error> {
        if offset == self.pos { return Ok(()); }
        let delta = i64::try_from(i128::from(offset) - i128::from(self.pos)).map_err(|_| Mp3Error::Corrupt("file offset out of range"))?;
        self.r.seek_relative(delta).map_err(io_err(what))?;
        self.pos = offset;
        Ok(())
    }
}

fn read_exact_at(src: &mut Rd, offset: u64, buf: &mut [u8], what: &'static str) -> Result<(), Mp3Error> {
    src.seek(offset, what)?;
    src.r.read_exact(buf).map_err(io_err(what))?;
    src.pos += buf.len() as u64;
    Ok(())
}

fn read_up_to(src: &mut Rd, offset: u64, buf: &mut [u8], what: &'static str) -> Result<usize, Mp3Error> {
    src.seek(offset, what)?;
    let mut n = 0;
    while n < buf.len() {
        match src.r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Mp3Error::Io(what, e)),
        }
    }
    src.pos += n as u64;
    Ok(n)
}

fn header_at(src: &mut Rd, offset: u64, len: u64) -> Result<Option<Header>, Mp3Error> {
    if offset.checked_add(4).is_none_or(|end| end > len) { return Ok(None); }
    let mut word = [0u8; 4];
    read_exact_at(src, offset, &mut word, "read MP3 header")?;
    Ok(Header::parse(u32::from_be_bytes(word)))
}

fn same_stream(a: &Header, b: &Header) -> bool {
    a.layer == b.layer && a.version == b.version && a.sample_rate == b.sample_rate && a.channels() == b.channels()
}

/// Offset of an Xing/Info marker inside a frame of this layout (VBRI is always at byte 36).
fn tag_offset(h: &Header) -> usize { 4 + h.side_info_bytes() }

fn be32(b: &[u8]) -> u32 { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) }

/// CRC-16 (polynomial 0xA001, reflected, initial value 0) of the LAME info tag.
fn lame_crc(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= u16::from(b);
        for _ in 0..8 { crc = if crc & 1 != 0 { (crc >> 1) ^ 0xa001 } else { crc >> 1 }; }
    }
    crc
}

fn parse_tag(frame: &[u8], h: &Header) -> Result<Tag, Mp3Error> {
    let o = tag_offset(h);
    let marker = frame.get(o..o + 4);
    if matches!(marker, Some(b"Xing") | Some(b"Info")) {
        let info = marker == Some(b"Info");
        let flags = frame.get(o + 4..o + 8).map(be32).ok_or(Mp3Error::Unsupported("truncated Xing/Info header"))?;
        let mut at = o + 8;
        let mut frames = None;
        let mut bytes = None;
        if flags & 1 != 0 { frames = Some(frame.get(at..at + 4).map(be32).ok_or(Mp3Error::Unsupported("truncated Xing/Info header"))?); at += 4; }
        if flags & 2 != 0 { bytes = Some(frame.get(at..at + 4).map(be32).ok_or(Mp3Error::Unsupported("truncated Xing/Info header"))?); at += 4; }
        if flags & 4 != 0 { at += 100; }
        if flags & 8 != 0 { at += 4; }
        // The LAME extension carries the 12-bit encoder delay and padding at +21. A header whose
        // identifier is "LAME" is trusted; any other writer's must carry the right info-tag CRC.
        let ext = frame.get(at + 21..at + 24).ok_or(Mp3Error::Unsupported("Xing/Info header without room for gapless fields"))?;
        let packed = (u32::from(ext[0]) << 16) | (u32::from(ext[1]) << 8) | u32::from(ext[2]);
        let lame = frame.get(at..at + 4) == Some(b"LAME");
        let crc_ok = frame.get(at + 34..at + 36).is_some_and(|c| lame_crc(&frame[..at + 34]) == u16::from_be_bytes([c[0], c[1]]));
        let (delay, padding) = if lame || crc_ok { (packed >> 12, packed & 0xfff) } else { (0, 0) };
        return Ok(Tag::Xing { info, lame, frames, bytes, delay, padding, size: frame.len() as u32 });
    }
    // A Fraunhofer VBRI header sits at byte 36 of the frame whatever the layout (MPEG version,
    // channel mode, CRC); AudioToolbox accepts it in all of them and ignores its version, delay
    // and quality fields. The stream byte count is at 46 and the frame count at 50.
    if frame.get(36..40) == Some(b"VBRI") {
        let fields = frame.get(46..54).ok_or(Mp3Error::Unsupported("truncated VBRI header"))?;
        return Ok(Tag::Vbri { bytes: be32(&fields[..4]), frames: be32(&fields[4..]), size: frame.len() as u32 });
    }
    Ok(Tag::None)
}

/// Skip ID3v2 tags at the start; returns the offset after them.
fn skip_id3v2(src: &mut Rd, len: u64) -> Result<u64, Mp3Error> {
    let mut pos = 0u64;
    loop {
        let mut head = [0u8; 10];
        if pos.checked_add(10).is_none_or(|end| end > len) { return Ok(pos); }
        read_exact_at(src, pos, &mut head, "read ID3v2 header")?;
        if &head[..3] != b"ID3" { return Ok(pos); }
        if head[3] == 0xff || head[4] == 0xff || head[6..10].iter().any(|b| b & 0x80 != 0) {
            return Err(Mp3Error::Unsupported("malformed ID3v2 header"));
        }
        let size = (u64::from(head[6]) << 21) | (u64::from(head[7]) << 14) | (u64::from(head[8]) << 7) | u64::from(head[9]);
        let footer = head[3] >= 4 && head[5] & 0x10 != 0;
        pos += 10 + size + if footer { 10 } else { 0 };
        if pos > len { return Err(Mp3Error::Corrupt("ID3v2 tag extends past the end of the file")); }
    }
}

struct Scan {
    frames: u64,
    bitrate_sum: u64,
    /// Header bit rates (kbit/s) of the first (up to) ten frames and their count.
    head_sum: u64,
    head_count: u64,
    first_bitrate: u32,
    end: u64,
}

/// Stream features whose AudioToolbox output was not matched are refused: mixed
/// long/short blocks (AudioToolbox and the standard decoders disagree on them)
/// and MPEG-2/2.5 intensity stereo.
fn check_side_info(h: &Header, side: &[u8]) -> Result<(), Mp3Error> {
    let lsf = h.version != Version::V1;
    if lsf && h.mode == 1 && h.mode_ext & 1 != 0 { return Err(Mp3Error::Unsupported("MPEG-2 intensity stereo")); }
    let channels = h.channels();
    let granules = if lsf { 1 } else { 2 };
    // Offsets of the first granule/channel, the per-granule-channel stride and the
    // window_switching_flag position inside it.
    let (mut pos, stride, switching) = if lsf {
        (8 + if channels == 1 { 1 } else { 2 }, 63usize, 38usize)
    } else {
        (9 + if channels == 1 { 5 } else { 3 } + 4 * channels, 59, 33)
    };
    let bit = |i: usize| side.get(i / 8).map_or(0, |b| (b >> (7 - i % 8)) & 1);
    for _ in 0..granules * channels {
        if bit(pos + switching) == 1 {
            let block_type = (bit(pos + switching + 1) << 1) | bit(pos + switching + 2);
            if block_type == 2 && bit(pos + switching + 3) == 1 { return Err(Mp3Error::Unsupported("mixed long/short blocks")); }
        }
        pos += stride;
    }
    Ok(())
}

/// Count the contiguous run of frames starting at `start`. The run must be
/// uniform (same version, layer, rate, channel count) and free of free-format
/// frames; a trailing partial frame is ignored.
fn scan_frames(src: &mut Rd, start: u64, len: u64, first: &Header) -> Result<Scan, Mp3Error> {
    let mut pos = start;
    let mut scan = Scan { frames: 0, bitrate_sum: 0, head_sum: 0, head_count: 0, first_bitrate: 0, end: start };
    loop {
        let Some(h) = header_at(src, pos, len)? else { break };
        if !same_stream(&h, first) { break; }
        let size = h.frame_bytes() as u64;
        if size == 0 { return Err(Mp3Error::Unsupported("free-format MPEG audio")); }
        if pos + size > len { break; }
        let crc_len = if h.crc { 2usize } else { 0 };
        let side_len = h.side_info_bytes();
        if size < (4 + crc_len + side_len) as u64 { return Err(Mp3Error::Unsupported("MP3 frame shorter than its side information")); }
        let mut side = [0u8; 32];
        read_exact_at(src, pos + 4 + crc_len as u64, &mut side[..side_len], "read MP3 side information")?;
        check_side_info(&h, &side[..side_len])?;
        if scan.frames == 0 { scan.first_bitrate = h.bitrate_kbps; }
        scan.frames += 1;
        scan.bitrate_sum += u64::from(h.bitrate_kbps);
        if scan.head_count < 10 { scan.head_sum += u64::from(h.bitrate_kbps); scan.head_count += 1; }
        pos += size;
        scan.end = pos;
    }
    Ok(scan)
}

/// True when the bytes at `start` are the beginning of a frame of the stream (a header cut short, or a
/// header whose frame does not fit in the file): a truncated final frame.
fn is_partial_frame(src: &mut Rd, start: u64, len: u64, first: &Header) -> Result<bool, Mp3Error> {
    if start >= len { return Ok(false); }
    let take = (len - start).min(4) as usize;
    let mut word = [0u8; 4];
    read_exact_at(src, start, &mut word[..take], "read trailer")?;
    if take < 4 {
        let expected = first.raw.to_be_bytes();
        // Sync, version and layer (protection bit free), then the sampling-rate bits.
        return Ok(word[0] == 0xff
            && (take < 2 || word[1] & 0xfe == expected[1] & 0xfe)
            && (take < 3 || word[2] & 0x0c == expected[2] & 0x0c));
    }
    Ok(Header::parse(u32::from_be_bytes(word)).is_some_and(|h| {
        let size = h.frame_bytes() as u64;
        same_stream(&h, first) && size != 0 && start + size > len
    }))
}

/// After the last frame only a truncated frame, an ID3v1/APEv2 tag, padding or
/// bytes that cannot start a frame are accepted.
fn check_trailer(src: &mut Rd, start: u64, len: u64, first: &Header) -> Result<(), Mp3Error> {
    if is_partial_frame(src, start, len, first)? { return Ok(()); }
    let mut end = len;
    // ID3v1
    if end >= start + 128 {
        let mut tag = [0u8; 3];
        read_exact_at(src, end - 128, &mut tag, "read ID3v1")?;
        if &tag == b"TAG" { end -= 128; }
    }
    // APEv2 footer
    if end >= start + 32 {
        let mut footer = [0u8; 32];
        read_exact_at(src, end - 32, &mut footer, "read APE footer")?;
        if &footer[..8] == b"APETAGEX" {
            let size = u64::from(u32::from_le_bytes([footer[12], footer[13], footer[14], footer[15]]));
            let has_header = u32::from_le_bytes([footer[20], footer[21], footer[22], footer[23]]) & 0x8000_0000 != 0;
            let total = size + if has_header { 32 } else { 0 };
            if total <= end - start { end -= total; }
        }
    }
    let mut pos = start;
    let mut buf = vec![0u8; 64 * 1024];
    while pos < end {
        let want = ((end - pos).min(buf.len() as u64)) as usize;
        let n = read_up_to(src, pos, &mut buf[..want], "read trailer")?;
        if n == 0 { break; }
        // Windows overlap by one byte so a 0xFF at the end is paired with its successor.
        let last = if pos + (n as u64) < end { n - 1 } else { n };
        for i in 0..last {
            if buf[i] != 0xff { continue; }
            let second = if i + 1 < n { buf[i + 1] } else { 0 };
            if second & 0xe0 == 0xe0 {
                // Something that could be a frame: only fine if it is a header whose frame does not fit.
                let at = pos + i as u64;
                if let Some(h) = header_at(src, at, len)? {
                    let size = h.frame_bytes() as u64;
                    if size == 0 || at + size <= len { return Err(Mp3Error::Unsupported("audio data after a gap in the MP3 stream")); }
                } else {
                    return Err(Mp3Error::Unsupported("ambiguous data after the MP3 stream"));
                }
            }
        }
        pos += last.max(1) as u64;
    }
    Ok(())
}

/// True when a run of `complete` frames (at least one) may end at `pos`: the file ends, or an ID3v1 tag
/// follows, or (after two frames) the next frame is cut short.
fn stream_may_end_at(src: &mut Rd, pos: u64, len: u64, first: &Header, complete: u32) -> Result<bool, Mp3Error> {
    if len.saturating_sub(pos) < 4 { return Ok(true); }
    if len - pos == 128 {
        let mut tag = [0u8; 3];
        read_exact_at(src, pos, &mut tag, "read ID3v1")?;
        if &tag == b"TAG" { return Ok(true); }
    }
    Ok(complete >= 2 && is_partial_frame(src, pos, len, first)?)
}

/// Free-format frames have no length in their header: the stream is confirmed by a second free-format
/// header of the same stream within the longest plausible frame.
fn confirm_free_format(src: &mut Rd, at: u64, len: u64, first: &Header) -> Result<bool, Mp3Error> {
    let mut buf = vec![0u8; ((len - at).min(4100)) as usize];
    let n = read_up_to(src, at, &mut buf, "scan free-format frame")?;
    Ok((4..n.saturating_sub(3)).any(|i| {
        buf[i] == 0xff && Header::parse(be32(&buf[i..i + 4])).is_some_and(|h| h.frame_bytes() == 0 && same_stream(&h, first))
    }))
}

/// A candidate header starts a confirmed MP3 stream when up to three consecutive complete frames carry
/// headers of one stream (same version, layer, sampling rate and channel mode; the bit rate may change)
/// and the data then continues, or may end there (end of file, a cut frame, an ID3v1 tag). Everything
/// else that merely looks like a frame header is not an MP3 stream.
fn confirm_stream(src: &mut Rd, at: u64, len: u64, first: &Header) -> Result<bool, Mp3Error> {
    if first.frame_bytes() == 0 { return confirm_free_format(src, at, len, first); }
    let (mut pos, mut complete) = (at, 0u32);
    while complete < 3 {
        if complete > 0 && stream_may_end_at(src, pos, len, first, complete)? { return Ok(true); }
        let Some(h) = header_at(src, pos, len)?.filter(|h| same_stream(h, first) && h.frame_bytes() != 0) else { return Ok(false) };
        let size = h.frame_bytes() as u64;
        if pos + size > len { return Ok(false); }
        pos += size;
        complete += 1;
    }
    Ok(true)
}

impl Mp3Stream {
    /// Probe and open. `NotMp3` means the data has no MP3 signature and the
    /// caller should report its own format error.
    pub fn open(file: File) -> Result<Mp3Stream, Mp3Error> {
        let len = file.metadata().map_err(io_err("inspect MP3"))?.len();
        let mut src = Rd::new(file);
        // A broken ID3v2 tag leaves no frame to confirm.
        let after_id3 = match skip_id3v2(&mut src, len) {
            Err(Mp3Error::Unsupported(_) | Mp3Error::Corrupt(_)) => return Err(Mp3Error::NotMp3),
            other => other?,
        };

        // ---- locate the first frame. Anything that is not a confirmed MP3 stream is `NotMp3`, so the
        // caller's own classification and error stand. Junk before the stream is skipped (bounded)
        // unless it contains something that looks like a frame header: AudioToolbox deviates then.
        let mut first_pos = None;
        let mut ambiguous = false;
        let mut pos = after_id3;
        let mut window = vec![0u8; 4096];
        'search: while pos < len && pos - after_id3 <= LEADING_JUNK_LIMIT {
            let n = read_up_to(&mut src, pos, &mut window, "scan for MP3 frame")?;
            if n == 0 { break; }
            for i in 0..n {
                if window[i] != 0xff { continue; }
                let next = if i + 1 < n { window[i + 1] } else {
                    let mut b = [0u8; 1];
                    if pos + (n as u64) < len { read_exact_at(&mut src, pos + n as u64, &mut b, "scan for MP3 frame")?; }
                    b[0]
                };
                if next & 0xe0 != 0xe0 { continue; }
                let at = pos + i as u64;
                match header_at(&mut src, at, len)? {
                    Some(h) if confirm_stream(&mut src, at, len, &h)? => { first_pos = Some((at, h)); break 'search; }
                    _ => ambiguous = true,
                }
            }
            pos += n as u64;
        }
        let Some((first_at, first)) = first_pos else { return Err(Mp3Error::NotMp3) };
        if ambiguous { return Err(Mp3Error::Unsupported("ambiguous data before the first MP3 frame")); }
        if first.layer != 3 { return Err(Mp3Error::Unsupported("MPEG audio Layer I/II")); }
        if first.frame_bytes() == 0 { return Err(Mp3Error::Unsupported("free-format MPEG audio")); }
        let spf = first.samples_per_frame() as u64;

        // ---- tag frame
        let first_size = first.frame_bytes();
        let mut frame = vec![0u8; first_size];
        read_exact_at(&mut src, first_at, &mut frame, "read first MP3 frame")?;
        let tag = parse_tag(&frame, &first)?;
        let audio_start = if matches!(tag, Tag::None) { first_at } else { first_at + first_size as u64 };

        let scan = scan_frames(&mut src, audio_start, len, &first)?;
        if scan.frames == 0 { return Err(Mp3Error::Corrupt("MP3 stream holds no audio frames")); }
        check_trailer(&mut src, scan.end, len, &first)?;
        let n = scan.frames;

        // ---- Apple's packet count, gapless fields and property values
        let rate = u64::from(first.sample_rate);
        let channels = first.channels() as u32;
        let (packets, delay, padding, bitrate, capped) = match tag {
            Tag::None => {
                let mean = (scan.bitrate_sum / n) as u32;
                (n, 0u64, 0u64, mean * 1000, true)
            }
            Tag::Vbri { bytes, frames, size } => {
                // A frame count of 0 or 1 (the VBRI frame itself) leaves AudioToolbox with the scanned
                // frames and their mean rate; otherwise the packet count is frames - 1.
                let mean = |sum: u64, count: u64| (sum / count) as u32 * 1000;
                let scanned_mean = mean(scan.bitrate_sum, n);
                if frames < 2 {
                    (n, 0, 0, scanned_mean, false)
                } else {
                    let packets = u64::from(frames) - 1;
                    // bits / seconds over the stream bytes (the tag frame's own size is not counted when
                    // the byte field exceeds it); an unusable byte count or a result of 0 falls back to
                    // the mean rate of the first ten frames.
                    let head_mean = mean(scan.head_sum, scan.head_count);
                    let payload = if bytes > size { bytes - size } else { bytes };
                    let seconds = (packets as f64 * spf as f64) / rate as f64;
                    let derived = (f64::from(payload) * 8.0 / seconds) as u32;
                    (packets, 0, 0, if bytes == 0 || derived == 0 { head_mean } else { derived }, false)
                }
            }
            Tag::Xing { info, lame, frames, bytes, delay, padding, size } => {
                // A LAME "Info" (CBR) header gets its packet count from the byte count; any other
                // header uses its frame count.
                let by_bytes = info && lame;
                let mut info_bitrate = None;
                let packets = if by_bytes {
                    let bytes = bytes.ok_or(Mp3Error::Unsupported("Info header without a byte count"))?;
                    if bytes <= size { return Err(Mp3Error::Unsupported("Info byte count is smaller than its own frame")); }
                    // Seconds of audio at the stream bitrate, then packets: this operation order
                    // reproduced AudioToolbox's rounding at exact-integer boundaries.
                    let bits = f64::from(bytes - size) * 8.0;
                    let seconds = bits / (f64::from(scan.first_bitrate) * 1000.0);
                    let estimate = (seconds * (f64::from(first.sample_rate) / spf as f64)).ceil();
                    if !(1.0..4.0e9).contains(&estimate) { return Err(Mp3Error::Unsupported("implausible Info byte count")); }
                    // AudioToolbox's bitrate property is bits / seconds with this same duration; in binary64
                    // that quotient lands one bit/s under the nominal rate for some byte counts, and so does it.
                    info_bitrate = Some((bits / seconds) as u32);
                    estimate as u64
                } else {
                    match frames { Some(f) if f != 0 => u64::from(f), _ => return Err(Mp3Error::Unsupported("Xing/Info header without a frame count")) }
                };
                let brat = if let Some(derived) = info_bitrate { derived } else {
                    let b = bytes.ok_or(Mp3Error::Unsupported("Xing/Info header without a byte count"))?;
                    if b <= size { return Err(Mp3Error::Unsupported("Xing byte count is smaller than its own frame")); }
                    // bits / seconds with the duration formed as packets * frames_per_packet / rate in binary64,
                    // which differs from exact integer arithmetic by one bit/s when the quotient is integral.
                    let seconds = (packets as f64 * spf as f64) / rate as f64;
                    let value = f64::from(b - size) * 8.0 / seconds;
                    if !(0.0..4.0e9).contains(&value) { return Err(Mp3Error::Unsupported("implausible Xing byte count")); }
                    value as u32
                };
                (packets, u64::from(delay), u64::from(padding), brat, true)
            }
        };
        let length_frames = (packets * spf) as i64 - (delay + padding) as i64;
        if length_frames <= 0 { return Err(Mp3Error::Unsupported("MP3 gapless header leaves no samples")); }
        let decoded_packets = if matches!(tag, Tag::Vbri { .. }) { n } else { packets.min(n) };
        let raw_total = decoded_packets * spf;
        let skip = delay + DECODER_DELAY;
        if delay >= raw_total || skip >= raw_total + DECODER_DELAY { return Err(Mp3Error::Unsupported("MP3 encoder delay exceeds the stream")); }
        let mut out_len = raw_total - delay;
        if capped { out_len = out_len.min(length_frames as u64); }
        let info = Mp3Info { sample_rate: first.sample_rate, channels, frames_per_packet: spf as u32, length_frames, bitrate };
        let mut stream = Mp3Stream {
            src, info, first, next_frame: audio_start, frames_total: decoded_packets, frames_decoded: 0,
            decoder: Decoder::new(), pcm: Pcm::new(), cur: [Vec::with_capacity(2 * GRANULE), Vec::with_capacity(2 * GRANULE)],
            cur_start: 0, skip, out_len, out_pos: 0, frame_buf: Vec::with_capacity(1500), spf, channels: channels as usize,
            seen_good_frame: false,
        };
        stream.cur[0].clear();
        Ok(stream)
    }

    pub fn info(&self) -> Mp3Info { self.info }

    /// Decode the next frame into `cur`, replacing the previous one.
    fn load_frame(&mut self) -> Result<(), Mp3Error> {
        let mut word = [0u8; 4];
        read_exact_at(&mut self.src, self.next_frame, &mut word, "read MP3 frame")?;
        let Some(h) = Header::parse(u32::from_be_bytes(word)).filter(|h| same_stream(h, &self.first)) else {
            return Err(Mp3Error::Corrupt("MP3 frame header changed after the file was scanned"));
        };
        let size = h.frame_bytes();
        if size < 4 { return Err(Mp3Error::Corrupt("MP3 frame changed after the file was scanned")); }
        self.frame_buf.resize(size, 0);
        read_exact_at(&mut self.src, self.next_frame, &mut self.frame_buf, "read MP3 frame")?;
        self.next_frame += size as u64;
        let status = self.decoder.decode_frame(&h, self.frame_buf.get(4..).unwrap_or_default(), &mut self.pcm)
            .map_err(|_| Mp3Error::Corrupt("truncated MP3 frame"))?;
        match status.damage {
            None => self.seen_good_frame = true,
            // Frames that still need bytes from before the start of the stream decode as silence.
            Some(Damage::ReservoirShort) if !self.seen_good_frame => {}
            Some(_) => return Err(Mp3Error::Corrupt("MP3 frame data is damaged")),
        }
        if status.crc_mismatch { return Err(Mp3Error::Corrupt("MP3 frame CRC mismatch")); }
        let count = status.granules * GRANULE;
        self.cur_start = self.frames_decoded * self.spf;
        for ch in 0..2 {
            let source = self.pcm.data[ch.min(self.channels - 1)];
            self.cur[ch].clear();
            self.cur[ch].extend(source[..count].iter().map(|&v| quantize(v)));
        }
        self.frames_decoded += 1;
        Ok(())
    }

    /// Frames still deliverable.
    pub fn remaining(&self) -> u64 { self.out_len - self.out_pos }

    /// Deliver up to `left.len()` frames as two planes (the second duplicates
    /// the first for mono); returns the number delivered, 0 at the end.
    pub fn read_planar(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<usize, Mp3Error> {
        let want = left.len().min(right.len()).min(self.remaining().min(usize::MAX as u64) as usize);
        let raw_total = self.frames_total * self.spf;
        let mut done = 0usize;
        while done < want {
            let g = self.skip + self.out_pos;
            if g >= raw_total {
                // Past the decoded packets: the reader pads with silence.
                let n = want - done;
                left[done..done + n].fill(0.0);
                right[done..done + n].fill(0.0);
                done += n;
                self.out_pos += n as u64;
                continue;
            }
            let cur_end = self.cur_start + self.cur[0].len() as u64;
            if self.cur[0].is_empty() || g >= cur_end {
                self.load_frame()?;
                continue;
            }
            let offset = (g - self.cur_start) as usize;
            let n = (self.cur[0].len() - offset).min(want - done);
            left[done..done + n].copy_from_slice(&self.cur[0][offset..offset + n]);
            right[done..done + n].copy_from_slice(&self.cur[1][offset..offset + n]);
            done += n;
            self.out_pos += n as u64;
        }
        Ok(done)
    }
}

/// Round to the 16-bit grid with saturation and return it as a float, the way
/// AudioToolbox's integer decoder output reaches the float client format.
fn quantize(v: f64) -> f32 {
    let scaled = (v * 32768.0 + 0.5).floor();
    let clamped = if scaled.is_nan() { 0.0 } else { scaled.clamp(-32768.0, 32767.0) };
    (clamped as i32) as f32 * (1.0 / 32768.0)
}

#[cfg(test)]
#[path = "mp3_tests.rs"]
mod tests;
